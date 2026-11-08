#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check enforcing BUILD.bazel minimality, Starlark buildifier
# formatting/lint hygiene, and strict target-directory scope:
#    - redundant default attributes in BUILD.bazel:
#      * crate_name = "<x>" when identical to target name (modulo '-' vs '_'), or
#        on a crate unit-test rustc_test unless disambiguating the bazel2gn GN
#        output_name (e.g. <lib_crate_name>_test)
#      * edition on a rustc_test that sets `crate` (inherited from the crate); a
#        kept `crate_name` next to `crate` needs a comment saying what needs the
#        GN output_name (rustc_test_crate_name_missing_comment, warning)
#      * version = "0.1.0" on first-party (in-tree) rustc_* targets
#      * crate_root = "src/lib.rs" on rustc_library
#      * crate_root = "src/main.rs" on rustc_binary
#      * lint_config matching the Fuchsia Rust wrapper macro default
#        (//build/config/rust/lints:clippy_warn_default on testonly rustc_* and
#        test targets, or //build/config/rust/lints:clippy_warn_production when
#        inline unit tests are not enabled)
#      * output_name = "<x>" or out = "<x>" when identical to target name (and
#        unsupported output_name on go_binary / go_binary_host_tool)
#      * unsupported manifest attributes (`component_name`, `manifest`, `includes`,
#        `restricted_features`) passed to `fx_component` or `fx_test_component`
#        instead of `fx_component_manifest`
#      * commented-out dependency or source labels (`# "//..."`, `# ":..."`) in
#        BUILD.bazel list attributes (`deps`, `srcs`, etc.) carried over from BUILD.gn
#    - missing //build/bazel/versioning:is_api_level_PLATFORM visibility in
#      build/bazel/versioning/BUILD.bazel when calling legacy host-tool macros
#      (go_binary_host_tool, py_binary_host_tool)
#    - unwrapped external C/C++ or Rust rules (cc_library, cc_binary from
#      @rules_cc//, or rust_library, rust_binary, rust_proc_macro, rust_test,
#      rust_shared_library, rust_dylib_library from @rules_rust//) when
#      Fuchsia-specific wrapper macros (fx_cc_library, fx_cc_binary, rustc_*)
#      exist under //build/bazel/rules/
#    - private or duplicate external rule load() statements (@...//.../private/...)
#    and genrule commands added by the change that derive paths or arguments with
#    shell command substitution ($$(dirname ...), $$(python3 -c ...), backticks) or
#    hard-code bazel-out/ paths instead of using Bazel's predefined genrule variables.
# 2. Runs `buildifier -lint=warn -format=json -mode=check` (with an AST fallback
#    for `.bzl` module docstrings) on all changed and target-package Starlark
#    files (`BUILD.bazel`, `BUILD`, `*.bzl`, `*.bazel`, `MODULE.bazel`),
#    catching missing `.bzl` module docstrings, unused `load` symbols, and
#    unformatted Starlark files before `shac` runs in CQ.
# 3. Flags drive-by edits to unrelated packages and shared Bazel rule/macro
#    definitions (`build/bazel/rules/**`, `build/bazel/aspects/**`, etc.) outside
#    PLANTER_TARGET_DIR / PLANTER_TARGET_DIRS (except centralized registration
#    lists, ancestor BUILD.gn/BUILD.bazel test groups/suites, and dependency
#    packages the change migrates: a new BUILD.bazel referenced, transitively,
#    from a target package's BUILD.bazel, and the in-tree build of
#    third_party/rust_crates: its BUILD files, Cargo.toml/Cargo.lock and compat/
#    shims).
#    TODO: Evaluate each out-of-scope edit for validity/usefulness and apply or
#    batch valid changes in a separate commit.
# 4. Audits compiled_action() targets, Starlark action rule definitions under
#    build/bazel/rules/, and bazel2gn compiled_action converters (rejecting
#    depfile/deps $(location ...) substitutions, attr.output on GN-only forwarded
#    attributes, missing explicit tool_output_name, and speculative auto-inference).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"

python3 - "$WORKDIR" "$TARGET_DIRS" "$CHANGE_BASE" <<'PYEOF'
import ast
import difflib
import json
import os
import re
import shutil
import subprocess
import sys

# Normalize target directory arguments, mapping '.' and empty entries to ''.
def _normalize_target_dir(raw_dir: str) -> str:
    d = (raw_dir or "").strip()
    while d.startswith("//") or d.startswith("./"):
        d = d[2:]
    d = d.strip("/")
    return "" if d == "." else d


workdir = os.path.abspath(sys.argv[1])
raw_dirs = sys.argv[2]
change_base = sys.argv[3].strip() or "HEAD"
target_dirs = [
    norm_d
    for d in re.split(r"[\s,]+", raw_dirs)
    if (norm_d := _normalize_target_dir(d))
]

ALLOWED_GLOBAL_PREFIXES = (
    "build/bazel2gn",
    "build/tools/bazel2gn/",
    "build/bazel/",
    "build/bazel_sdk/",
    "build/rust/",
    "build/config/rust/lints/",
    "bundles/assembly/",
    "build/images/",
    "sdk/history/",
)

RUST_CRATES = "third_party/rust_crates/"

SHARED_BAZEL_RULE_PREFIXES = (
    "build/bazel/rules/",
    "build/bazel/aspects/",
    "build/bazel/toolchains/",
    "build/bazel/starlark/",
    "build/bazel/scripts/",
)


def is_shared_bazel_rule_file(norm: str) -> bool:
    if not target_dirs:
        return False
    if any(
        p.strip("/").startswith(("build/tools/bazel2gn/", "build/bazel2gn/"))
        for p in changed
    ):
        return False
    if any(norm == td or norm.startswith(td + "/") for td in target_dirs):
        return False
    if norm.startswith(SHARED_BAZEL_RULE_PREFIXES):
        return True
    if norm.startswith("build/bazel/") and norm.endswith(".bzl"):
        return not norm.startswith("build/bazel/update-rustc-third-party/")
    return False


def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


def git_out(args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        ).strip()
    except Exception:
        return ""


def _changed_code_lines(diff_lines):
    """Returns stripped, non-empty, non-comment `+`/`-` lines from a unified diff."""
    changed = [
        ln[1:].strip()
        for ln in diff_lines
        if ln[:1] in ("+", "-") and not ln.startswith(("+++", "---"))
    ]
    return [ln for ln in changed if ln and not ln.startswith("#")]


def is_ancestor(anc: str, desc: str) -> bool:
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


def normalize_git_rev(rev: str) -> str:
    """Map jj relative revsets (@, @-, @--) to git HEAD equivalents and advance stale pre-rebase bases."""
    rev = (rev or "").strip() or "HEAD"
    m = re.fullmatch(r"@(-*)", rev)
    if m:
        rev = f"HEAD~{len(m.group(1))}" if m.group(1) else "HEAD"
    head_list = git_lines(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
    base_list = git_lines(["rev-parse", "--verify", "--quiet", f"{rev}^{{commit}}"])
    orig_list = git_lines(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    head_sha = head_list[0] if head_list else ""
    base_sha = base_list[0] if base_list else ""
    orig_sha = orig_list[0] if orig_list else ""
    if head_sha and orig_sha:
        mb_list = git_lines(["merge-base", orig_sha, head_sha])
        merge_base = mb_list[0] if mb_list else ""
        if base_sha and merge_base and base_sha != merge_base:
            is_anc = (
                subprocess.run(
                    ["git", "-C", workdir, "merge-base", "--is-ancestor", base_sha, merge_base],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                ).returncode
                == 0
            )
            if is_anc:
                return merge_base
        if base_sha and merge_base:
            in_head = (
                subprocess.run(
                    ["git", "-C", workdir, "merge-base", "--is-ancestor", base_sha, head_sha],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                ).returncode
                == 0
            )
            if not in_head:
                return merge_base
    return rev


# Advance change_base to merge-base(origin/main, HEAD) or HEAD~1 when HEAD was rebased
# onto a newer origin/main after PLANTER_CHANGE_BASE was pinned.
def resolve_effective_change_base(raw_base: str) -> str:
    base = normalize_git_rev(raw_base)
    head_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
    parent_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"])
    base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"])
    origin_main = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])
    merge_base = (
        git_out(["merge-base", origin_main, head_sha]) if (origin_main and head_sha) else ""
    )
    if (
        base_sha
        and merge_base
        and base_sha != merge_base
        and is_ancestor(base_sha, merge_base)
    ):
        base = merge_base
        base_sha = merge_base
    elif head_sha and base_sha and not is_ancestor(base_sha, head_sha):
        base = merge_base or "HEAD~1"
        base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"])
    # Never step back to HEAD~1 when HEAD itself is already merged on origin/main.
    head_on_origin = bool(origin_main and head_sha and is_ancestor(head_sha, origin_main))
    head_subj = (git_lines(["log", "-1", "--format=%s", "HEAD"]) or [""])[0]
    parent_subj = (git_lines(["log", "-1", "--format=%s", "HEAD~1"]) or [""])[0]
    mig_prefixes = ("[bazel_migration]", "[bazel2gn]", "[bazel]")
    if parent_sha and not head_on_origin and head_subj.startswith(mig_prefixes):
        head_files = git_lines(["diff", "--name-only", "HEAD~1", "HEAD"])
        if not target_dirs or any(
            any(p == td or p.startswith(td + "/") for td in target_dirs) for p in head_files
        ):
            parent_on_origin = bool(origin_main and is_ancestor(parent_sha, origin_main))
            head_cids = re.findall(
                r"(?mi)^Change-Id:\s*(I[0-9a-f]+)",
                git_out(["log", "-1", "--format=%B", "HEAD"]),
            )
            parent_cids = re.findall(
                r"(?mi)^Change-Id:\s*(I[0-9a-f]+)",
                git_out(["log", "-1", "--format=%B", "HEAD~1"]),
            )
            distinct_stack_cl = bool(
                head_cids and parent_cids and head_cids[-1] != parent_cids[-1]
            )
            stack_files = (
                git_lines(["diff", "--name-only", base_sha, "HEAD~1"])
                if (base_sha and base_sha != parent_sha and is_ancestor(base_sha, parent_sha))
                else []
            )
            stack_touches_tdirs = bool(target_dirs) and any(
                any(p == td or p.startswith(td + "/") for td in target_dirs)
                for p in stack_files
            )
            parent_is_upstream = not parent_subj.startswith(mig_prefixes)
            if (
                base == "HEAD"
                or base_sha == head_sha
                or (
                    base_sha
                    and base_sha != parent_sha
                    and is_ancestor(base_sha, parent_sha)
                    and (
                        parent_is_upstream
                        or not origin_main
                        or parent_on_origin
                        or distinct_stack_cl
                        or not stack_touches_tdirs
                    )
                )
            ):
                return "HEAD~1"
    return base


def _emit(findings, category, severity, file, line, message, remediation=""):
    d = {
        "source": "bazel_minimality",
        "category": category,
        "severity": severity,
        "file": file,
        "line": line,
        "message": message,
    }
    if remediation:
        d["remediation"] = remediation
    findings.append(d)


# Resolve changed and untracked files consistently even when PLANTER_CHANGED_FILES is set.
_resolved_env = os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
_raw_base = _resolved_env or change_base
_raw_base_sha = git_out(["rev-parse", "--verify", "--quiet", f"{_raw_base}^{{commit}}"])
_head_sha = git_out(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
_rebased_since_pin = bool(
    _raw_base_sha and _head_sha and not is_ancestor(_raw_base_sha, _head_sha)
)
change_base = resolve_effective_change_base(_raw_base)
# Invalidate PLANTER_CHANGED_FILES when resolve_effective_change_base advances
# change_base (e.g. from merge-base to HEAD~1 on a stacked prerequisite branch)
# so parent-commit files are not flagged as out-of-scope edits in the child CL.
_resolved_base_sha = git_out(
    ["rev-parse", "--verify", "--quiet", f"{change_base}^{{commit}}"]
)
_base_advanced = bool(
    _raw_base_sha and _resolved_base_sha and _raw_base_sha != _resolved_base_sha
)
_env_changed = (
    None
    if (_rebased_since_pin or _base_advanced)
    else os.environ.get("PLANTER_CHANGED_FILES")
)
untracked = [
    f
    for f in git_lines(["ls-files", "--others", "--exclude-standard"])
    if not f.startswith("third_party/bazel_vendor/")
    and "/__pycache__/" not in f
    and not f.startswith("__pycache__/")
    and not f.endswith(".pyc")
    and ("/" in f or not re.fullmatch(r"CoderReport[\w.-]*\.json", f))
]
if _env_changed is not None:
    changed = {
        l.strip()
        for l in _env_changed.splitlines()
        if l.strip()
        and ("/" in l.strip() or not re.fullmatch(r"CoderReport[\w.-]*\.json", l.strip()))
    }
    untracked = [f for f in untracked if f in changed]
else:
    changed = set(git_lines(["diff", "--name-only", change_base]))
    changed.update(untracked)


_RUST_LINT_CONFIG_WRAPPER_RULES = (
    "rustc_library",
    "rustc_binary",
    "rustc_dylib",
    "rustc_proc_macro",
    "rustc_embed_files",
    "rustc_binary_host_tool",
    "idk_rustc_binary_host_tool",
    "ffx_tool",
    "idk_ffx_tool",
    "ffx_plugin",
    "rustc_test",
    "host_rustc_test",
)


def _redundant_rust_lint_config_info(call_node: ast.Call, r_name: str, p_dir: str):
    """Returns (raw_lint_cfg, lineno, is_testonly_target) when lint_config matches r_name's default."""
    if r_name not in _RUST_LINT_CONFIG_WRAPPER_RULES:
        return None
    l_cfg = None
    lineno = getattr(call_node, "lineno", 1)
    for kw in call_node.keywords:
        if (
            kw.arg == "lint_config"
            and isinstance(kw.value, ast.Constant)
            and isinstance(kw.value.value, str)
        ):
            l_cfg = kw.value.value
            lineno = getattr(kw.value, "lineno", getattr(kw, "lineno", lineno))
            break
    if l_cfg is None:
        return None
    stripped = l_cfg.lstrip("@")
    if stripped.startswith(":") and p_dir == "build/config/rust/lints":
        norm_l_cfg = f"//build/config/rust/lints{stripped}"
    else:
        norm_l_cfg = stripped
    # ffx_plugin neither declares nor forwards `testonly` or `with_*_unit_tests`;
    # its four inner rustc_library subtargets always default to clippy_warn_production.
    is_testonly_target = r_name in ("rustc_test", "host_rustc_test") or (
        r_name != "ffx_plugin"
        and any(
            kw.arg == "testonly"
            and isinstance(kw.value, ast.Constant)
            and kw.value.value is True
            for kw in call_node.keywords
        )
    )
    has_inline_unit_tests = r_name != "ffx_plugin" and any(
        kw.arg in ("with_unit_tests", "with_host_unit_tests")
        and isinstance(kw.value, ast.Constant)
        and kw.value.value is True
        for kw in call_node.keywords
    )
    default_lint_cfg = (
        "//build/config/rust/lints:clippy_warn_default"
        if is_testonly_target
        else (
            "//build/config/rust/lints:clippy_warn_production"
            if not has_inline_unit_tests
            else ""
        )
    )
    if default_lint_cfg and norm_l_cfg == default_lint_cfg:
        return (l_cfg, lineno, is_testonly_target)
    return None


def get_pkg_area(pkg: str) -> str:
    parts = [p for p in pkg.strip("/").split("/") if p]
    if not parts:
        return ""
    if parts[0] in ("src", "sdk", "tools", "zircon", "third_party") and len(parts) >= 2:
        return f"{parts[0]}/{parts[1]}"
    return parts[0]


def third_party_project(pkg: str) -> str:
    parts = [p for p in pkg.strip("/").split("/") if p]
    if "third_party" not in parts:
        return ""
    idx = parts.index("third_party")
    return "/".join(parts[: idx + 2])


def is_same_third_party_scope(pkg_a: str, pkg_b: str) -> bool:
    return third_party_project(pkg_a) == third_party_project(pkg_b)


def package_eliminated_gn_or_sentinel(pkg: str) -> bool:
    gn_path = os.path.join(workdir, pkg, "BUILD.gn")
    if not os.path.isfile(gn_path):
        return True
    if f"{pkg}/BUILD.gn" not in changed:
        return False
    try:
        with open(gn_path, encoding="utf-8") as f:
            return "BAZEL2GN SENTINEL" not in f.read()
    except OSError:
        return False


# Extract top-level target rule `name = "..."` definitions from Starlark text,
# ignoring `package(...)` and `exports_files(...)` metadata calls.
def extract_bazel_target_names(bz_text: str, filename: str = "BUILD.bazel") -> set[str]:
    if not bz_text.strip():
        return set()
    names: set[str] = set()
    try:
        tree = ast.parse(bz_text, filename=filename)
        for stmt in tree.body:
            if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
                continue
            func = stmt.value.func
            if isinstance(func, ast.Name) and func.id in ("package", "exports_files"):
                continue
            for kw in stmt.value.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    names.add(kw.value.value)
    except SyntaxError:
        for m in re.finditer(r'^\s*name\s*=\s*"([^"]+)"', bz_text, re.M):
            names.add(m.group(1))
    return names


# Check whether `<pkg>` already had a `BUILD.bazel` or `BUILD` file at `change_base`
# (or `HEAD~1` when `HEAD` is a migration commit rebased onto newer upstream history).
def had_bazel_pkg_at_base(pkg: str) -> bool:
    refs = [change_base]
    if change_base != "HEAD~1":
        refs.append("HEAD~1")
    for ref in refs:
        for bfile in ("BUILD.bazel", "BUILD"):
            if git_lines(["ls-tree", "--name-only", ref, "--", f"{pkg}/{bfile}"]):
                return True
    return False


# Check whether `<pkg>` was unmigrated at `change_base` (no `BUILD.bazel`/`BUILD`, an
# `exports_files`-only stub, or missing a target in `req_targets` that `<pkg>/BUILD.bazel`
# now migrates).
def was_unmigrated_dep_at_base(pkg: str, req_targets: set[str]) -> bool:
    if not had_bazel_pkg_at_base(pkg):
        return True
    base_names: set[str] = set()
    for bfile in ("BUILD.bazel", "BUILD"):
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{pkg}/{bfile}"]))
        if base_txt:
            base_names |= extract_bazel_target_names(base_txt, f"{pkg}/{bfile}")
    if not base_names:
        return True
    if not req_targets:
        return False
    cur_txt = ""
    for bfile in ("BUILD.bazel", "BUILD"):
        bz_path = os.path.join(workdir, pkg, bfile)
        if os.path.isfile(bz_path):
            try:
                with open(bz_path, encoding="utf-8") as f:
                    cur_txt += "\n" + f.read()
            except OSError:
                pass
    cur_names = extract_bazel_target_names(cur_txt, f"{pkg}/BUILD.bazel")
    return bool((req_targets & cur_names) - base_names)


# Check whether a C/C++ library rule call only exposes headers/include paths without
# compiling source files.
def _is_header_only_cc_call(call: ast.Call) -> bool:
    code_exts = (".c", ".cc", ".cpp", ".cxx", ".m", ".mm", ".s", ".S", ".rs", ".go", ".py")
    for kw in call.keywords:
        if kw.arg == "srcs":
            for sub in ast.walk(kw.value):
                if (
                    isinstance(sub, ast.Constant)
                    and isinstance(sub.value, str)
                    and sub.value.endswith(code_exts)
                ):
                    return False
    return True


# Check whether `<pkg>/BUILD.bazel` is a minimal file-export, manifest-shard, or
# header-only C/C++ interface package (with `BUILD.gn` untouched) exposing checked-in
# files, `.shard.cml` shards, or include directories to an in-scope target without
# compiling sources or migrating `BUILD.gn`.
def is_upstream_file_or_header_stub_pkg(pkg: str) -> bool:
    if not pkg or f"{pkg}/BUILD.gn" in changed:
        return False
    bz_path = os.path.join(workdir, pkg, "BUILD.bazel")
    try:
        with open(bz_path, encoding="utf-8") as f:
            tree = ast.parse(f.read(), filename=f"{pkg}/BUILD.bazel")
    except (OSError, SyntaxError):
        return False
    allowed_meta = (
        "load",
        "package",
        "licenses",
        # Package-local `license(...)` (rules_license) declarations are metadata: third-party
        # stubs must attribute their own license text instead of the top-level license.
        "license",
        "exports_files",
        "filegroup",
        "cml_shard",
        "expect_includes",
        "fx_runner_shard",
    )
    header_cc_rules = ("cc_library", "fx_cc_library", "fx_cc_library_headers")
    saw_stub = False
    for stmt in tree.body:
        if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Constant):
            continue
        if not (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
        ):
            return False
        fn_name = stmt.value.func.id
        if fn_name in allowed_meta:
            if fn_name in (
                "exports_files",
                "filegroup",
                "cml_shard",
                "expect_includes",
                "fx_runner_shard",
            ):
                saw_stub = True
            continue
        if fn_name in header_cc_rules and _is_header_only_cc_call(stmt.value):
            saw_stub = True
            continue
        return False
    return saw_stub


# Check whether `<pkg>/BUILD.bazel` is a pure `exports_files(...)` stub (with `BUILD.gn`
# untouched) added only to expose checked-in files across Bazel package boundaries.
def is_exports_files_only_bazel_pkg(pkg: str) -> bool:
    if not pkg or f"{pkg}/BUILD.gn" in changed:
        return False
    bz_path = os.path.join(workdir, pkg, "BUILD.bazel")
    try:
        with open(bz_path, encoding="utf-8") as f:
            tree = ast.parse(f.read(), filename=f"{pkg}/BUILD.bazel")
    except (OSError, SyntaxError):
        return False
    saw_exports = False
    for stmt in tree.body:
        if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Constant):
            continue
        if not (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
            and stmt.value.func.id in ("package", "exports_files")
        ):
            return False
        if stmt.value.func.id == "exports_files":
            saw_exports = True
    return saw_exports


# Collect workspace package paths -> referenced target names in `<pkg>/BUILD.bazel`
# outside `visibility` attributes so upstream dependencies and exported file packages
# can be identified.
def collect_non_visibility_dep_targets(pkg: str) -> dict[str, set[str]]:
    bz_path = os.path.join(workdir, pkg, "BUILD.bazel")
    try:
        with open(bz_path, encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return {}
    label_re = re.compile(
        r"(?<![A-Za-z0-9_.:/\-])@?//([A-Za-z0-9_.\-/]+)(?::([A-Za-z0-9_.\-/]+))?"
    )
    deps: dict[str, set[str]] = {}
    try:
        tree = ast.parse(text, filename=f"{pkg}/BUILD.bazel")
        str_vals: list[str] = []
        for stmt in tree.body:
            if isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call):
                is_load = (
                    isinstance(stmt.value.func, ast.Name)
                    and stmt.value.func.id == "load"
                )
                for kw in stmt.value.keywords:
                    if kw.arg in ("visibility", "default_visibility"):
                        continue
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            str_vals.append(sub.value)
                for idx, pos_arg in enumerate(stmt.value.args):
                    if is_load and idx > 0:
                        continue
                    for sub in ast.walk(pos_arg):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            str_vals.append(sub.value)
            elif isinstance(stmt, ast.Assign):
                for sub in ast.walk(stmt.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        str_vals.append(sub.value)
        scan_items = str_vals
    except SyntaxError:
        scan_items = [re.sub(r"#[^\n]*", "", text)]
    for item in scan_items:
        for m in label_re.finditer(item):
            ref, raw_tgt = m.group(1).strip("/"), m.group(2) or ""
            if not ref or raw_tgt in ("__pkg__", "__subpackages__"):
                continue
            tgt = raw_tgt or os.path.basename(ref)
            deps.setdefault(ref, set())
            if tgt and not tgt.endswith(".bzl"):
                deps[ref].add(tgt)
    return deps


def collect_non_visibility_dep_pkgs(pkg: str) -> set[str]:
    return set(collect_non_visibility_dep_targets(pkg).keys())


# Check whether an existing `<pkg>/BUILD.bazel` (already present with targets at `change_base`)
# only has collateral edits needed by a downstream migration (such as `visibility`, `test_suite`
# registration, or replacing legacy `HOST_CONSTRAINTS` with `HOST_OS_CONSTRAINTS`).
def is_collateral_existing_bazel_edit(pkg: str) -> bool:
    if not pkg or not had_bazel_pkg_at_base(pkg):
        return False
    rel_bz = f"{pkg}/BUILD.bazel"
    base_ref = change_base
    base_txt = "\n".join(git_lines(["show", f"{base_ref}:{rel_bz}"]))
    if (not base_txt or not extract_bazel_target_names(base_txt, rel_bz)) and base_ref != "HEAD~1":
        alt_txt = "\n".join(git_lines(["show", f"HEAD~1:{rel_bz}"]))
        if alt_txt and extract_bazel_target_names(alt_txt, rel_bz):
            base_ref = "HEAD~1"
            base_txt = alt_txt
    if not base_txt or not extract_bazel_target_names(base_txt, rel_bz):
        return False
    diff_lines = git_lines(["diff", "--unified=0", base_ref, "--", rel_bz])
    if not diff_lines and base_ref == "HEAD":
        diff_lines = git_lines(["diff", "--unified=0", "HEAD~1", "HEAD", "--", rel_bz])
    changed_lines = _changed_code_lines(diff_lines)
    if not changed_lines:
        return True
    collateral_pat = re.compile(
        r'^(?:load\(\s*"(?:@platforms//host:constraints\.bzl|'
        r"//build/bazel/platforms:constraints\.bzl|"
        r'//build/bazel/rules/[\w/]+:(?:defs|\w+)\.bzl)"|'
        r"target_compatible_with\s*=\s*HOST_(?:OS_)?CONSTRAINTS\b|"
        r"visibility\s*=\s*|"
        r'"(?:(?://|:)[^"]+|\w+)"\s*,?$|'
        r"\],?$|\),?$)"
    )
    return all(collateral_pat.search(ln) for ln in changed_lines)


# Collect the transitive first-party workspace package closure reachable from `seed_pkgs`
# (seeds included) through non-visibility BUILD.bazel labels, skipping third_party/,
# bundles/, and global build-infrastructure packages, so collateral or surgical edits on
# direct or transitive upstream dependencies (e.g. from a stacked parent commit after
# rebase) are recognized.
def collect_transitive_dep_pkgs(seed_pkgs, max_visited: int = 500) -> set[str]:
    visited: set[str] = set()
    queue = [p for p in dict.fromkeys(seed_pkgs) if p]
    while queue and len(visited) < max_visited:
        cur = queue.pop(0)
        if cur in visited:
            continue
        visited.add(cur)
        for dep_pkg in sorted(collect_non_visibility_dep_pkgs(cur)):
            if (
                dep_pkg
                and dep_pkg not in visited
                and not dep_pkg.startswith(ALLOWED_GLOBAL_PREFIXES)
                and not dep_pkg.startswith(("third_party/", "bundles/"))
            ):
                queue.append(dep_pkg)
    return visited


def migrated_dependency_packages():
    """Returns (dependency_dirs, required_upstream_deps) for packages outside target_dirs.

    Per //build/beads/references/migration/scoping_changelists.md Section 2 & Section 3:
    - Never co-migrate cross-area packages, third_party/ packages (with first-party), or
      sprawling multi-hop closures exceeding CL scoping limits.
    - Allowed co-migrations:
      1. Subdirectories of an in-scope migrated directory (`cand_pkg.startswith(td + "/")`, e.g.
         internal helpers `//<dir>/<helper>` or direct test dirs `//<dir>/tests`, Patterns 2.B & 2.D).
      2. Unmigrated upstream dependency packages (`td` -> ... -> `cand_pkg`) in the same
         third_party scope that had no migrated BUILD.bazel at `change_base` and must be
         co-migrated in this change so `target_dirs` can resolve its Bazel dependencies without
         failing `cq_reachability` (`missing_bazel_dependency_package`) or `fx bazel build`.
      3. A sole same-area GN consumer of `td` ONLY when co-migrating it eliminates `td/BUILD.gn` or
         avoids `bazel2gn` in `td/BUILD.gn` (Pattern 2.A).
      4. An unmigrated same-area dependency package required to compile an in-scope target in Bazel
         (even when `bazel2gn` is kept for remaining GN consumers, or when `<pkg>/BUILD.bazel` at
         base was only an `exports_files` stub / partial migration missing the depended-on target),
         or a same-area private helper whose `BUILD.gn` or sentinel is eliminated (Pattern 2.B).
    """
    existing_changed = {
        p for p in changed if os.path.isfile(os.path.join(workdir, p))
    }
    added = (
        set(git_lines(["diff", "--name-only", "--diff-filter=AM", change_base]))
        | set(untracked)
        | existing_changed
    )
    candidates = {
        os.path.dirname(p)
        for p in changed
        if os.path.basename(p) == "BUILD.bazel"
        and not p.strip("/").startswith(ALLOWED_GLOBAL_PREFIXES)
        and not p.strip("/").startswith("bundles/")
        and not is_exports_files_only_bazel_pkg(os.path.dirname(p))
        and not is_collateral_existing_bazel_edit(os.path.dirname(p))
    }
    candidates -= set(target_dirs)
    if not candidates or not target_dirs:
        return set(), set()
    found: set[str] = set()
    required_upstream: set[str] = set()
    progressed = True
    while progressed:
        progressed = False
        active_dep_targets: dict[str, set[str]] = {}
        in_scope_pkgs = list(dict.fromkeys(list(target_dirs) + sorted(found)))
        for src_pkg in in_scope_pkgs:
            for dep_pkg, dep_tgts in collect_non_visibility_dep_targets(src_pkg).items():
                active_dep_targets.setdefault(dep_pkg, set()).update(dep_tgts)
        for cand_pkg in sorted(candidates - found):
            # 1. Child/descendant directory of an in-scope migrated directory (Patterns 2.B & 2.D)
            if any(
                cand_pkg.startswith(td + "/") and is_same_third_party_scope(cand_pkg, td)
                for td in in_scope_pkgs
            ):
                found.add(cand_pkg)
                if cand_pkg in active_dep_targets and was_unmigrated_dep_at_base(
                    cand_pkg, active_dep_targets[cand_pkg]
                ):
                    required_upstream.add(cand_pkg)
                progressed = True
                continue
            # 1b. Direct FIDL library dependency in //sdk/fidl/<lib> upgraded from legacy
            # fuchsia_fidl_library (C++ only) to fidl_library so Rust/C++ bindings exist in Bazel.
            if cand_pkg in active_dep_targets and cand_pkg.startswith("sdk/fidl/"):
                found.add(cand_pkg)
                required_upstream.add(cand_pkg)
                progressed = True
                continue
            # 2. Required unmigrated upstream dependency package in the same third_party scope
            if (
                cand_pkg in active_dep_targets
                and any(is_same_third_party_scope(cand_pkg, td) for td in in_scope_pkgs)
                and was_unmigrated_dep_at_base(cand_pkg, active_dep_targets[cand_pkg])
            ):
                found.add(cand_pkg)
                required_upstream.add(cand_pkg)
                progressed = True
                continue
            # Must be in the same area and third_party scope as at least one target_dir
            matching_tds = [
                td
                for td in target_dirs
                if get_pkg_area(cand_pkg) == get_pkg_area(td)
                and is_same_third_party_scope(cand_pkg, td)
            ]
            if not matching_tds:
                continue
            # 3. Pattern 2.A: Sole same-area consumer of td eliminating td/BUILD.gn or bazel2gn
            cand_texts = []
            for fn in ("BUILD.gn", "BUILD.bazel"):
                rel_f = f"{cand_pkg}/{fn}"
                cand_texts.extend(git_lines(["show", f"{change_base}:{rel_f}"]))
                try:
                    with open(os.path.join(workdir, rel_f), encoding="utf-8") as f:
                        cand_texts.append(f.read())
                except OSError:
                    pass
            joined = "\n".join(cand_texts)
            if any(
                re.search(
                    r'(?:@gn_targets//(?:[^\s"\':]+/)?|//)'
                    + re.escape(td)
                    + r'(?=[:"/\s\'])',
                    joined,
                )
                and package_eliminated_gn_or_sentinel(td)
                for td in matching_tds
            ):
                found.add(cand_pkg)
                progressed = True
                continue
            # 4. Pattern 2.B: Same-area dependency whose BUILD.gn or sentinel is eliminated
            if cand_pkg in active_dep_targets and package_eliminated_gn_or_sentinel(cand_pkg):
                found.add(cand_pkg)
                progressed = True
    return found, required_upstream


dependency_dirs, required_upstream_deps = migrated_dependency_packages()
in_scope_dep_pkgs: set[str] = set()
for _src_pkg in dict.fromkeys(list(target_dirs) + sorted(dependency_dirs)):
    in_scope_dep_pkgs |= collect_non_visibility_dep_pkgs(_src_pkg)
transitive_dep_pkgs = collect_transitive_dep_pkgs(
    list(dict.fromkeys(list(target_dirs) + sorted(dependency_dirs)))
)
newly_added_files = (
    set(git_lines(["diff", "--name-only", "--diff-filter=A", change_base]))
    | set(untracked)
    | {
        p
        for p in changed
        if os.path.basename(p) in ("BUILD.bazel", "BUILD")
        and os.path.isfile(os.path.join(workdir, p))
        and not had_bazel_pkg_at_base(os.path.dirname(p))
    }
)


def referenced_shared_configs():
    """Returns (allowed_gni_files, allowed_lint_pkgs) referenced by in-scope packages."""
    gni_files, lint_pkgs = set(), set()
    for td in list(target_dirs) + sorted(dependency_dirs):
        try:
            with open(os.path.join(workdir, td, "BUILD.gn"), encoding="utf-8") as f:
                gn_txt = f.read()
            for m in re.finditer(r'\bimport\(\s*"//([^"]+\.gni)"\s*\)', gn_txt):
                gni_files.add(m.group(1).strip("/"))
        except OSError:
            pass
        try:
            with open(os.path.join(workdir, td, "BUILD.bazel"), encoding="utf-8") as f:
                bz_txt = f.read()
            for m in re.finditer(r'\blint_config\s*=\s*"//([^":]+)', bz_txt):
                lint_pkgs.add(m.group(1).strip("/"))
        except OSError:
            pass
    return gni_files, lint_pkgs


allowed_gni_files, allowed_lint_pkgs = referenced_shared_configs()


def references_in_scope_package_at_base(norm: str) -> bool:
    """True when `norm` (or its package's BUILD.gn at `change_base`) references an in-scope migrated package."""
    in_scope = list(target_dirs) + sorted(dependency_dirs)
    if not in_scope:
        return False
    pkg_dir = os.path.dirname(norm).strip("/")
    if pkg_dir.endswith("/meta"):
        pkg_dir = os.path.dirname(pkg_dir).strip("/")
    candidates = [norm]
    if pkg_dir:
        for alt in (os.path.join(pkg_dir, "BUILD.gn"), os.path.join(pkg_dir, "BUILD.bazel")):
            if alt not in candidates:
                candidates.append(alt)
    texts = []
    for cand in candidates:
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{cand}"]))
        if base_txt:
            texts.append(base_txt)
        try:
            with open(os.path.join(workdir, cand), encoding="utf-8") as f:
                texts.append(f.read())
        except OSError:
            pass
    if not texts:
        return False
    combined = "\n".join(texts)
    for td in in_scope:
        pat = r'(?:@gn_targets//(?:[^\s"\':]+/)?|//)' + re.escape(td) + r'(?=[:"/\s\'])'
        if re.search(pat, combined):
            return True
    return False


# Parse disabled (`None`) attributes from symbolic `macro(...)` definitions under build/bazel/rules.
def load_disabled_symbolic_macro_attrs() -> dict[str, tuple[str, set[str]]]:
    rules_dir = os.path.join(workdir, "build", "bazel", "rules")
    if not os.path.isdir(rules_dir):
        return {}
    disabled_by_macro: dict[str, tuple[str, set[str]]] = {}
    for root, _, filenames in os.walk(rules_dir):
        for fn in sorted(filenames):
            if not fn.endswith(".bzl"):
                continue
            bzl_abs = os.path.join(root, fn)
            bzl_rel = os.path.relpath(bzl_abs, workdir).replace(os.sep, "/")
            try:
                with open(bzl_abs, "r", encoding="utf-8") as bf:
                    bzl_tree = ast.parse(bf.read(), filename=bzl_rel)
            except Exception:
                continue
            top_dicts: dict[str, dict[str, bool]] = {}
            helper_fns: dict[str, ast.FunctionDef] = {}

            def _eval_attr_map(node, depth: int = 0) -> dict[str, bool]:
                if depth > 6 or node is None:
                    return {}
                if isinstance(node, ast.Dict):
                    out: dict[str, bool] = {}
                    for k_n, v_n in zip(node.keys, node.values):
                        if isinstance(k_n, ast.Constant) and isinstance(k_n.value, str):
                            out[k_n.value] = (
                                isinstance(v_n, ast.Constant) and v_n.value is None
                            )
                    return out
                if isinstance(node, ast.Name):
                    return dict(top_dicts.get(node.id, {}))
                if isinstance(node, ast.BinOp) and isinstance(node.op, ast.BitOr):
                    merged = _eval_attr_map(node.left, depth + 1)
                    merged.update(_eval_attr_map(node.right, depth + 1))
                    return merged
                if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
                    h_fn = helper_fns.get(node.func.id)
                    if h_fn is not None:
                        ret_map: dict[str, bool] = {}
                        for sub in ast.walk(h_fn):
                            if isinstance(sub, ast.Return) and sub.value is not None:
                                ret_map.update(_eval_attr_map(sub.value, depth + 1))
                        return ret_map
                return {}

            for stmt in bzl_tree.body:
                if isinstance(stmt, ast.FunctionDef):
                    helper_fns[stmt.name] = stmt
                elif (
                    isinstance(stmt, ast.Assign)
                    and len(stmt.targets) == 1
                    and isinstance(stmt.targets[0], ast.Name)
                ):
                    sym_name = stmt.targets[0].id
                    val_n = stmt.value
                    if (
                        isinstance(val_n, ast.Call)
                        and isinstance(val_n.func, ast.Name)
                        and val_n.func.id == "macro"
                    ):
                        if sym_name.startswith("_"):
                            continue
                        for kw_n in val_n.keywords:
                            if kw_n.arg == "attrs":
                                amap = _eval_attr_map(kw_n.value)
                                none_set = {k for k, is_n in amap.items() if is_n}
                                if none_set:
                                    disabled_by_macro[sym_name] = (
                                        f"//{bzl_rel}",
                                        none_set,
                                    )
                    else:
                        dmap = _eval_attr_map(val_n)
                        if dmap:
                            top_dicts[sym_name] = dmap
    return disabled_by_macro


DISABLED_SYMBOLIC_MACRO_ATTRS = load_disabled_symbolic_macro_attrs()

# Attributes declared on the companion `fx_component_manifest` (never on `fx_component` /
# `fx_test_component`), and attributes disabled (`None`) on the component symbolic macros.
FX_COMPONENT_MANIFEST_ONLY_ATTRS = (
    "component_name",
    "manifest",
    "includes",
    "restricted_features",
)
FX_COMPONENT_DISABLED_MACRO_ATTRS = (
    "is_test",
    "test_realm",
    "moniker",
    "is_driver",
)


# Check whether an already-existing upstream dependency BUILD.bazel only adds missing
# hdrs, exports_files, cml_shard/expect_includes targets, or in-scope visibility entries,
# or removes disabled/manifest-only attributes from pre-existing rules without rewriting
# other rule attributes.
def is_surgical_upstream_dep_repair(norm: str) -> bool:
    if os.path.basename(norm) != "BUILD.bazel" or norm in newly_added_files:
        return False
    pkg_dir = os.path.dirname(norm).strip("/")
    if not pkg_dir:
        return False
    referenced_by_td = pkg_dir in transitive_dep_pkgs
    if not referenced_by_td:
        dep_ref_re = re.compile(r'"@?//' + re.escape(pkg_dir) + r'(?=[:"/\s\'])')
        search_dirs = dict.fromkeys(
            list(target_dirs)
            + sorted(dependency_dirs)
            + [
                os.path.dirname(p.strip("/"))
                for p in changed
                if os.path.basename(p) == "BUILD.bazel" and p.strip("/") != norm
            ]
        )
        for td in search_dirs:
            if not td:
                continue
            try:
                with open(os.path.join(workdir, td, "BUILD.bazel"), encoding="utf-8") as f:
                    if dep_ref_re.search(f.read()):
                        referenced_by_td = True
                        break
            except OSError:
                continue
    if not referenced_by_td:
        return False
    base_bz = git_out(["show", f"{change_base}:{norm}"])
    if not base_bz:
        return False
    try:
        with open(os.path.join(workdir, norm), encoding="utf-8") as f:
            cur_bz = f.read()
        base_tree = ast.parse(base_bz, filename=norm)
        cur_tree = ast.parse(cur_bz, filename=norm)
    except Exception:
        return False
    if ("default_visibility" in base_bz) != ("default_visibility" in cur_bz):
        return False

    ancillary_repair_fns = (
        "load",
        "package",
        "exports_files",
        "licenses",
        "visibility",
        "cml_shard",
        "expect_includes",
        "fx_runner_shard",
    )
    equiv_rule_names = {
        "fx_cc_library": "cc_library",
        "fx_cc_binary": "cc_binary",
        "rustc_library": "rust_library",
        "rustc_binary": "rust_binary",
        "rustc_proc_macro": "rust_proc_macro",
        "rustc_test": "rust_test",
    }

    # Normalize wrapper rule names (e.g. fx_cc_library -> cc_library) when shac lint
    # requires replacing direct @rules_cc / @rules_rust loads on a touched BUILD.bazel.
    def _canon_repair_rule(fn: str) -> str:
        return equiv_rule_names.get(fn, fn)

    def _rule_map(tree):
        out = {}
        for stmt in tree.body:
            if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
                continue
            fn = stmt.value.func.id if isinstance(stmt.value.func, ast.Name) else ""
            if fn in ancillary_repair_fns:
                continue
            for kw in stmt.value.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    out[kw.value.value] = _canon_repair_rule(fn)
        return out

    if _rule_map(base_tree) != _rule_map(cur_tree):
        return False

    comp_manifest_attrs = set(FX_COMPONENT_MANIFEST_ONLY_ATTRS)
    comp_disabled_attrs = comp_manifest_attrs | set(FX_COMPONENT_DISABLED_MACRO_ATTRS)

    def _repairable_attrs_for_rule(fn: str) -> set[str]:
        extra = set(DISABLED_SYMBOLIC_MACRO_ATTRS.get(fn, ("", set()))[1])
        if fn in ("fx_component", "fx_test_component"):
            extra |= comp_disabled_attrs
        elif fn in ("fx_component_manifest", "fx_compiled_manifest"):
            extra |= comp_manifest_attrs
        elif _canon_repair_rule(fn) == "cc_library":
            extra.add("alwayslink")
        return {"hdrs", "visibility"} | extra

    def _non_repair_attrs(tree):
        snap = []
        for stmt in tree.body:
            if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
                continue
            fn = stmt.value.func.id if isinstance(stmt.value.func, ast.Name) else ""
            if fn in ancillary_repair_fns:
                continue
            skip_attrs = _repairable_attrs_for_rule(fn)
            kws = {
                kw.arg: ast.dump(kw.value)
                for kw in stmt.value.keywords
                if kw.arg not in skip_attrs
            }
            snap.append((_canon_repair_rule(fn), kws))
        return snap

    return _non_repair_attrs(base_tree) == _non_repair_attrs(cur_tree)


# Check whether a deleted out-of-package .cml file was referenced by an in-scope
# package's pre-migration BUILD.gn and relocated into that Bazel package.
def is_relocated_in_scope_cml(norm: str) -> bool:
    if not norm.endswith(".cml") or os.path.exists(os.path.join(workdir, norm)):
        return False
    in_scope = list(target_dirs) + sorted(dependency_dirs)
    refs = [change_base, "HEAD~1"] if change_base == "HEAD" else [change_base]
    for td in in_scope:
        for ref in refs:
            gn_txt = "\n".join(git_lines(["show", f"{ref}:{td}/BUILD.gn"]))
            if not gn_txt:
                continue
            code_gn = re.sub(r"#[^\n]*", "", gn_txt)
            for raw_cml in re.findall(r'"([^"\n]+\.cml)"', code_gn):
                resolved = (
                    raw_cml[2:].lstrip("/")
                    if raw_cml.startswith("//")
                    else os.path.normpath(os.path.join(td, raw_cml)).replace("\\", "/")
                )
                if resolved == norm:
                    return True
    return False


# Allow registration-list .gni edits that only add or remove an in-scope package by its
# path relative to the .gni's directory (e.g. a `"<subdir>",` entry in a list of library
# subdirectories whose `<subdir>:tests` groups an ancestor BUILD.gn expands). Such lists
# name packages without a `//` label, so references_in_scope_package_at_base misses them.
def is_relative_name_registration_edit(norm: str) -> bool:
    if not norm.endswith(".gni"):
        return False
    gni_dir = os.path.dirname(norm)
    rel_names = set()
    for td in list(target_dirs) + sorted(dependency_dirs):
        if gni_dir and td.startswith(gni_dir + "/"):
            rel_names.add(td[len(gni_dir) + 1:])
    if not rel_names:
        return False
    diff = git_lines(["diff", "--unified=0", change_base, "--", norm])
    edited = _changed_code_lines(diff)
    if not edited:
        return False
    return all(
        re.fullmatch(r'"([^"]+)",?', ln) and re.fullmatch(r'"([^"]+)",?', ln).group(1) in rel_names
        for ln in edited
    )


# Allow collateral BUILD.bazel and .bzl rule/macro edits in upstream packages that
# replace `@platforms//host:constraints.bzl` (`HOST_CONSTRAINTS`) with
# `//build/bazel/platforms:constraints.bzl` (`HOST_OS_CONSTRAINTS`) (plus any
# `defs.bzl` -> direct rule `.bzl` load cleanup required by `scripts/shac/bazel_lint`
# on the modified file) so cross-compiled `host_linux_arm64` host tools build cleanly.
HOST_OS_UPGRADE_LINE_RE = re.compile(
    r'^(?:load\(\s*"(?:@platforms//host:constraints\.bzl|'
    r"//build/bazel/platforms:constraints\.bzl|"
    r'//build/bazel/rules/[\w/]+:(?:defs|\w+)\.bzl)"|'
    r"target_compatible_with\s*=\s*HOST_(?:OS_)?CONSTRAINTS\s*,?$|"
    r'"\w+"\s*,?$|'
    r"\),?$)"
)


def is_host_os_constraints_upgrade_edit(norm: str) -> bool:
    if os.path.basename(norm) not in ("BUILD.bazel", "BUILD") and not norm.endswith(".bzl"):
        return False
    pkg_d = os.path.dirname(norm).strip("/")
    base_ref = change_base
    if not pkg_d or not git_lines(["ls-tree", "--name-only", base_ref, "--", norm]):
        if (
            not pkg_d
            or base_ref == "HEAD~1"
            or not git_lines(["ls-tree", "--name-only", "HEAD~1", "--", norm])
        ):
            return False
        base_ref = "HEAD~1"
    diff = git_lines(["diff", "--unified=0", base_ref, "--", norm])
    if not diff and base_ref == "HEAD":
        diff = git_lines(["diff", "--unified=0", "HEAD~1", "HEAD", "--", norm])
    edited = _changed_code_lines(diff)
    if not edited or not any(
        "HOST_CONSTRAINTS" in ln or "HOST_OS_CONSTRAINTS" in ln for ln in edited
    ):
        return False
    return all(HOST_OS_UPGRADE_LINE_RE.search(ln) for ln in edited)


# Check whether a shared .bzl rule file under build/bazel/rules/ only adds a required
# bzl-load visibility([...]) entry not already covered by an existing prefix wildcard.
def is_required_bzl_visibility_allowlist_edit(norm: str) -> bool:
    base_bzl = git_out(["show", f"{change_base}:{norm}"])
    if not base_bzl or "visibility([" not in base_bzl:
        return False
    diff_lines = [
        dl
        for dl in git_lines(["diff", "--unified=0", change_base, "--", norm])
        if dl[:1] in ("+", "-") and not dl.startswith(("+++", "---"))
    ]
    if not diff_lines or any(dl.startswith("-") for dl in diff_lines):
        return False
    vis_m = re.search(r"\bvisibility\(\s*\[([^\]]*)\]\s*\)", base_bzl, re.S)
    base_vis = re.findall(r'"//([^"]+)"', vis_m.group(1)) if vis_m else []
    base_prefixes = [
        v[:-4].strip("/") for v in base_vis if v.endswith("/...")
    ]
    in_scope = list(target_dirs) + sorted(dependency_dirs)
    for dl in diff_lines:
        m = re.fullmatch(r'\+\s*"//([^"]+?)(?:/\.\.\.)?",?', dl.strip())
        if not m:
            return False
        added_pkg = m.group(1).strip("/")
        if not any(added_pkg == td or added_pkg.startswith(td + "/") for td in in_scope):
            return False
        if any(
            not pref or added_pkg == pref or added_pkg.startswith(pref + "/")
            for pref in base_prefixes
        ):
            return False
    return True


# Check whether `norm` belongs to the split that owners_allowlist_scope
# (owners_scoped_allowlist_misplaced) prescribes: moving an in-scope package's allowlist
# entry out of a parent `.bzl` into the OWNERS-gated child package `<sub>` (a `<sub>/*.bzl`
# naming an in-scope package, a metadata-only `<sub>/BUILD.bazel`, comment-only
# `LINT.IfChange` edits to `<sub>/BUILD.gn`, and the parent `.bzl` that loads and merges
# `//<sub>:<f>.bzl`).
def is_owners_scoped_allowlist_split(norm: str) -> bool:
    in_scope = list(target_dirs) + sorted(dependency_dirs)
    if not in_scope:
        return False
    ref_re = re.compile(
        "|".join(r"//" + re.escape(td) + r"(?=[:\"/\s'])" for td in in_scope)
    )

    def _read(rel):
        try:
            with open(os.path.join(workdir, rel), encoding="utf-8") as f:
                return f.read()
        except OSError:
            return ""

    changed_bzl = [c.strip("/") for c in changed if c.strip("/").endswith(".bzl")]

    def _valid_sub(sub):
        if not sub or not os.path.isfile(os.path.join(workdir, sub, "OWNERS")):
            return False
        parent = os.path.dirname(sub)
        load_re = re.compile(r'load\(\s*"//' + re.escape(sub) + r':[^"]+\.bzl"')
        if not any(
            os.path.dirname(c) == parent and load_re.search(_read(c)) for c in changed_bzl
        ):
            return False
        return any(os.path.dirname(c) == sub and ref_re.search(_read(c)) for c in changed_bzl)

    pkg = os.path.dirname(norm)
    base = os.path.basename(norm)
    if norm.endswith(".bzl"):
        if _valid_sub(pkg):
            return True
        for m in re.finditer(r'load\(\s*"//([^":]+):[^"]+\.bzl"', _read(norm)):
            sub = m.group(1).strip("/")
            if os.path.dirname(sub) == pkg and _valid_sub(sub):
                return True
        return False
    if not _valid_sub(pkg):
        return False
    if base == "BUILD.bazel":
        try:
            tree = ast.parse(_read(norm), filename=norm)
        except SyntaxError:
            return False
        return all(
            isinstance(st, ast.Expr)
            and isinstance(st.value, ast.Call)
            and isinstance(st.value.func, ast.Name)
            and st.value.func.id in ("load", "package", "licenses", "license", "exports_files")
            for st in tree.body
        )
    if base == "BUILD.gn":
        return all(
            not l.startswith(("+", "-"))
            or l.startswith(("+++", "---"))
            or not l[1:].strip()
            or l[1:].strip().startswith("#")
            for l in git_lines(["diff", "-U0", change_base, "--", norm])
        )
    return False


def is_allowed_scope(path: str) -> bool:
    if not target_dirs:
        return True
    norm = path.strip("/")
    pkg_dir = os.path.dirname(norm)
    for td in list(target_dirs) + sorted(dependency_dirs):
        if norm == td or norm.startswith(td + "/"):
            return True
        if (
            pkg_dir
            and td.startswith(pkg_dir + "/")
            and (
                os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel")
                or norm.endswith((".bzl", ".gni"))
            )
        ):
            return True
    if is_host_os_constraints_upgrade_edit(norm):
        return True
    if is_owners_scoped_allowlist_split(norm):
        return True
    if is_shared_bazel_rule_file(norm):
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{norm}"]))
        in_scope = list(target_dirs) + sorted(dependency_dirs)
        if base_txt and any(
            re.search(r"//"+ re.escape(td) + r'(?=[:"/\s\'])', base_txt)
            for td in in_scope
        ):
            return True
        return is_required_bzl_visibility_allowlist_edit(norm)
    if (
        os.path.basename(norm) in ("BUILD.bazel", "BUILD", "BUILD.gn")
        or norm.endswith((".bzl", ".gni", ".cml"))
    ) and references_in_scope_package_at_base(norm):
        # Collateral updates to an existing caller's BUILD.gn/BUILD.bazel (e.g. visibility,
        # test_suite registration, or bazel_host_tool wiring) are allowed, but full migration of
        # an out-of-scope package (creating <pkg_dir>/BUILD.bazel or adding BAZEL2GN SENTINEL to
        # <pkg_dir>/BUILD.gn) is forbidden unless <pkg_dir> is in target_dirs or dependency_dirs.
        if pkg_dir and not pkg_dir.startswith(ALLOWED_GLOBAL_PREFIXES) and not pkg_dir.startswith("bundles/"):
            if f"{pkg_dir}/BUILD.bazel" in newly_added_files:
                # A new file-export / header-only stub depended on by an in-scope package (whose
                # `visibility` names that package) is not a full migration of `pkg_dir`.
                return (
                    pkg_dir in in_scope_dep_pkgs
                    or pkg_dir in transitive_dep_pkgs
                ) and (
                    is_exports_files_only_bazel_pkg(pkg_dir)
                    or is_upstream_file_or_header_stub_pkg(pkg_dir)
                )
            base_gn = "\n".join(git_lines(["show", f"{change_base}:{pkg_dir}/BUILD.gn"]))
            if base_gn and "BAZEL2GN SENTINEL" not in base_gn:
                try:
                    with open(os.path.join(workdir, pkg_dir, "BUILD.gn"), encoding="utf-8") as f:
                        if "BAZEL2GN SENTINEL" in f.read():
                            return False
                except OSError:
                    pass
        return True
    if is_relocated_in_scope_cml(norm):
        return True
    # Allow pure `exports_files(...)` or header-only C/C++ interface BUILD.bazel stubs
    # that expose checked-in files/headers to in-scope migrated packages without
    # compiling sources or modifying that directory's BUILD.gn.
    if (
        os.path.basename(norm) == "BUILD.bazel"
        and (pkg_dir in in_scope_dep_pkgs or pkg_dir in transitive_dep_pkgs)
        and (
            is_exports_files_only_bazel_pkg(pkg_dir)
            or is_upstream_file_or_header_stub_pkg(pkg_dir)
        )
    ):
        return True
    if is_surgical_upstream_dep_repair(norm):
        return True
    if (
        os.path.basename(norm) == "BUILD.gn"
        and pkg_dir
        and is_surgical_upstream_dep_repair(f"{pkg_dir}/BUILD.bazel")
    ):
        return True
    # Allow collateral compatibility edits (such as visibility or replacing legacy
    # HOST_CONSTRAINTS with HOST_OS_CONSTRAINTS) on pre-existing transitive dependency
    # BUILD.bazel files (and any paired bazel2gn sync of their BUILD.gn).
    if (
        pkg_dir in transitive_dep_pkgs
        and os.path.basename(norm) in ("BUILD.bazel", "BUILD.gn")
        and is_collateral_existing_bazel_edit(pkg_dir)
    ):
        return True
    if norm in allowed_gni_files:
        return True
    if is_relative_name_registration_edit(norm):
        return True
    if pkg_dir in allowed_lint_pkgs and os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel"):
        return True
    for prefix in ALLOWED_GLOBAL_PREFIXES:
        if norm.startswith(prefix):
            return True
    if norm in (
        "build/BUILD.gn",
        "build/BUILD.bazel",
        "tools/BUILD.gn",
        "tools/BUILD.bazel",
        "src/BUILD.gn",
        "src/BUILD.bazel",
        "sdk/BUILD.gn",
        "sdk/BUILD.bazel",
        "sdk/atom_lists.bzl",
        "sdk/fidl/BUILD.gn",
        "sdk/fidl/category_lists.bzl",
    ):
        return True
    # Allow ancestor BUILD.gn / BUILD.bazel files of a collateral-modified package
    # (e.g. wiring a pre-existing test in a dependency package into its ancestor
    # test_suite when touching that dependency's BUILD.bazel triggers shac test parity).
    if os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel") and pkg_dir:
        for ch_path in changed:
            ch_norm = ch_path.strip("/")
            ch_pkg = os.path.dirname(ch_norm)
            if (
                ch_pkg
                and ch_pkg.startswith(pkg_dir + "/")
                and os.path.basename(ch_norm) in ("BUILD.gn", "BUILD.bazel")
                and references_in_scope_package_at_base(ch_norm)
            ):
                return True
    # Build files under bundles/ wire migrated targets and tests into CQ (test groups,
    # tests_barrier / bazel_target_test_suite_barrier of builder groups), which migrations
    # may need so exported bazel_test_suite tests reach tests.json only with their bundle.
    if norm.startswith("bundles/") and (
        os.path.basename(norm) in ("BUILD.gn", "BUILD.bazel") or norm.endswith(".gni")
    ):
        return True
    return is_rust_crates_build_file(norm)


RUST_CRATES = "third_party/rust_crates/"


def is_rust_crates_build_file(norm: str) -> bool:
    """The in-tree build of the vendored Rust crates, which migrations may fix.

    Their build files (generated from crate annotations under build/bazel/, already
    allowed), Cargo.toml/Cargo.lock, and compat/ shims are maintained in-tree. Vendored
    crate sources are not.
    """
    if not norm.startswith(RUST_CRATES):
        return False
    rest = norm[len(RUST_CRATES):]
    if rest in ("Cargo.toml", "Cargo.lock") or rest.startswith("compat/"):
        return True
    return os.path.basename(rest) in ("BUILD.gn", "BUILD.bazel")


findings = []

for path in sorted(changed):
    if not is_allowed_scope(path):
        if is_shared_bazel_rule_file(path.strip("/")):
            if path.strip("/") == "build/bazel/debug_symbols/aspects.bzl":
                shared_rem = (
                    f"Revert '{path}' (`git checkout {change_base} -- {path}`). When "
                    "`export_last_build_debug_symbols` fails because an assembled "
                    "`fx_package` packages a binary that depends on a `rustc_dylib` "
                    "(`rust_dylib_library`), do not edit `build/bazel/debug_symbols/aspects.bzl`; "
                    "instead remove the assembled `fx_package` target from the package's "
                    "`BUILD.bazel`, keep `fuchsia_package` above `## BAZEL2GN SENTINEL` in "
                    "`BUILD.gn` (while keeping `fx_component`, `fx_component_manifest`, and "
                    "`fx_packaged_binary` in `BUILD.bazel`), keep its "
                    "`//bundles/assembly/bazel_inputs/<dir>` bridge in "
                    "`bundles/assembly/BUILD.bazel` and `bundles/assembly/bazel_inputs/BUILD.gn`, "
                    "and remove the package target from `Bazel-Migration-Target:` footers and "
                    "commit-body migration claims."
                )
            else:
                shared_rem = (
                    f"Revert '{path}' (`git checkout {change_base} -- {path}`). If a migrated "
                    "target calls a legacy host-tool macro (`go_binary_host_tool`, "
                    "`py_binary_host_tool`) and fails visibility on "
                    "`//build/bazel/versioning:is_api_level_PLATFORM`, add `\"//<dir>:__pkg__\"` "
                    "to `is_api_level_PLATFORM`'s `visibility` list in "
                    "'build/bazel/versioning/BUILD.bazel' instead of editing "
                    f"'{path}'. If a target passes a redundant or unsupported attribute (such as "
                    "`output_name` matching `name` on `go_binary_host_tool`), omit or fix the "
                    "attribute in the target's BUILD.bazel."
                )
            _emit(
                findings,
                "out_of_scope_package_modified",
                "error",
                path,
                1,
                (
                    f"File '{path}' is a shared Bazel rule or macro definition outside the "
                    f"assigned target directory scope ({', '.join(target_dirs)}). Package "
                    "migrations must never modify shared rules or macros under '//build/bazel/' "
                    "(such as loading private rule definitions, wrapping macros, editing debug "
                    "symbol aspects, or adding custom attributes)."
                ),
                shared_rem,
            )
        else:
            out_msg = (
                f"File '{path}' is outside the assigned target directory scope "
                f"({', '.join(target_dirs)}). Per "
                "//build/beads/references/migration/scoping_changelists.md, do not sweep "
                "cross-area packages, general-purpose dependencies, transitive closures, or "
                "downstream consumers that do not eliminate BUILD.gn/bazel2gn into the same CL."
            )
            out_rem = (
                f"Revert changes to '{path}' (`git checkout {change_base} -- {path}` or "
                f"`rm -f {path}`) and restrict modifications strictly to the assigned target "
                "package(s) and allowed same-area co-migrated packages."
            )
            _emit(findings, "out_of_scope_package_modified", "error", path, 1, out_msg, out_rem)


# Count hand-authored added lines in `<norm>`, excluding machine-generated output below
# `## BAZEL2GN SENTINEL` in dual-built `BUILD.gn` files (already counted in `BUILD.bazel`).
def count_authored_added_lines(norm: str) -> int:
    full_p = os.path.join(workdir, norm)
    if os.path.basename(norm) == "BUILD.gn" and os.path.isfile(full_p):
        try:
            with open(full_p, encoding="utf-8", errors="ignore") as f:
                cur_gn = f.read()
        except OSError:
            cur_gn = ""
        if "BAZEL2GN SENTINEL" in cur_gn:
            base_gn = "\n".join(git_lines(["show", f"{change_base}:{norm}"]))
            base_pre = re.split(
                r"(?m)^##\s*BAZEL2GN SENTINEL", base_gn, maxsplit=1
            )[0].splitlines()
            cur_pre = re.split(
                r"(?m)^##\s*BAZEL2GN SENTINEL", cur_gn, maxsplit=1
            )[0].splitlines()
            sm = difflib.SequenceMatcher(a=base_pre, b=cur_pre)
            return sum(
                j2 - j1
                for tag, _i1, _i2, j1, j2 in sm.get_opcodes()
                if tag in ("insert", "replace")
            )
    if norm in untracked:
        try:
            with open(full_p, encoding="utf-8", errors="ignore") as f:
                return sum(1 for _ in f)
        except OSError:
            return 0
    return sum(
        1
        for dl in git_lines(["diff", "--unified=0", change_base, "--", norm])
        if dl.startswith("+") and not dl.startswith("+++")
    )


# Check whether a package's BUILD.bazel only declares @bazel2gn:skip bridge stubs
# (without modifying BUILD.gn or performing a full target migration).
def is_bridge_stub_bazel_package(pkg: str) -> bool:
    bz_path = os.path.join(workdir, pkg, "BUILD.bazel")
    if not os.path.isfile(bz_path) or f"{pkg}/BUILD.gn" in changed:
        return False
    try:
        with open(bz_path, encoding="utf-8") as f:
            bz_src = f.read()
        tree = ast.parse(bz_src, filename=bz_path)
    except Exception:
        return False
    src_lines = bz_src.splitlines()
    rule_calls = 0
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        fn = (
            stmt.value.func.id
            if isinstance(stmt.value.func, ast.Name)
            else (
                stmt.value.func.attr
                if isinstance(stmt.value.func, ast.Attribute)
                else ""
            )
        )
        if fn in ("load", "package", "exports_files", "licenses", "visibility"):
            continue
        rule_calls += 1
        idx = getattr(stmt.value, "lineno", 1) - 2
        has_skip = False
        while idx >= 0:
            s_ln = src_lines[idx].strip()
            if not s_ln:
                idx -= 1
                continue
            if s_ln.startswith("#"):
                if "@bazel2gn:skip" in s_ln:
                    has_skip = True
                idx -= 1
                continue
            break
        if not has_skip:
            return False
    return rule_calls > 0


# Check whether a package's BUILD.bazel only defines test_suite() targets
# (ancestor test-suite aggregation wiring rather than a migrated target package).
def is_test_suite_only_bazel_package(pkg: str) -> bool:
    bz_path = os.path.join(workdir, pkg, "BUILD.bazel")
    if not os.path.isfile(bz_path):
        return False
    try:
        with open(bz_path, encoding="utf-8") as f:
            tree = ast.parse(f.read(), filename=bz_path)
    except Exception:
        return False
    rule_calls = 0
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            continue
        fn = (
            stmt.value.func.id
            if isinstance(stmt.value.func, ast.Name)
            else (
                stmt.value.func.attr
                if isinstance(stmt.value.func, ast.Attribute)
                else ""
            )
        )
        if fn in ("load", "package", "exports_files", "licenses", "visibility"):
            continue
        if fn != "test_suite":
            return False
        rule_calls += 1
    return rule_calls > 0


# Resolve migrated packages that belong directly to an assigned target directory
# or its same-scope child subpackage hierarchy (Patterns 2.B & 2.D).
def assigned_subtree_packages(pkgs):
    return {
        pkg
        for pkg in pkgs
        if any(
            (pkg == td or pkg.startswith(td + "/"))
            and is_same_third_party_scope(pkg, td)
            for td in target_dirs
        )
    }


# Also treat co-migrated dependency packages reachable from assigned target directories
# (and their assigned subtree packages) as required upstream dependencies, in addition to
# the unmigrated upstream dependencies found by migrated_dependency_packages().
required_upstream_deps |= (
    collect_transitive_dep_pkgs(
        list(target_dirs) + sorted(assigned_subtree_packages(dependency_dirs))
    )
    & set(dependency_dirs)
) - set(target_dirs)


# Check whether a modified BUILD.bazel outside target_dirs only performs an ancillary
# edit (exports_files/header stub, surgical upstream repair, collateral visibility or
# HOST_OS_CONSTRAINTS edit on an existing package, or ancestor test_suite wiring).
def _is_ancillary_only_bazel_edit(norm: str) -> bool:
    pkg = os.path.dirname(norm.strip("/"))
    if not pkg:
        return True
    if (
        is_exports_files_only_bazel_pkg(pkg)
        or is_upstream_file_or_header_stub_pkg(pkg)
        or is_bridge_stub_bazel_package(pkg)
        or is_test_suite_only_bazel_package(pkg)
        or is_surgical_upstream_dep_repair(norm)
        or is_collateral_existing_bazel_edit(pkg)
        or is_host_os_constraints_upgrade_edit(norm)
    ):
        return True
    if any(td.startswith(pkg + "/") for td in target_dirs) and f"{pkg}/BUILD.gn" not in changed:
        base_bz = git_out(["show", f"{change_base}:{norm}"])
        if base_bz:
            base_non_suite = {
                n
                for n in extract_bazel_target_names(base_bz, norm)
                if not re.search(
                    r'\btest_suite\s*\([^)]*\bname\s*=\s*"' + re.escape(n) + r'"',
                    base_bz,
                    re.S,
                )
            }
            try:
                with open(os.path.join(workdir, norm), encoding="utf-8") as f:
                    cur_bz = f.read()
            except OSError:
                cur_bz = ""
            cur_non_suite = {
                n
                for n in extract_bazel_target_names(cur_bz, norm)
                if not re.search(
                    r'\btest_suite\s*\([^)]*\bname\s*=\s*"' + re.escape(n) + r'"',
                    cur_bz,
                    re.S,
                )
            }
            if cur_non_suite == base_non_suite:
                return True
    return False


# Enforce CL directory count (<= 5) and added-line count (<= ~500 for multi-directory CLs)
# per //build/beads/references/migration/scoping_changelists.md when discretionary
# co-migrated directories (consumers or siblings not required as upstream dependencies
# of target_dirs) are included.
def check_cl_scoping_guidelines():
    if not target_dirs:
        return
    migrated_pkgs = set()
    for p in sorted(changed):
        norm = p.strip("/")
        if os.path.basename(norm) != "BUILD.bazel":
            continue
        if norm.startswith(ALLOWED_GLOBAL_PREFIXES) or norm.startswith(
            ("bundles/", RUST_CRATES)
        ):
            continue
        pkg = os.path.dirname(norm)
        if not pkg:
            continue
        if pkg not in target_dirs and _is_ancillary_only_bazel_edit(norm):
            continue
        if norm in newly_added_files or pkg in target_dirs or pkg in dependency_dirs:
            migrated_pkgs.add(pkg)
    if len(migrated_pkgs) <= 1:
        return
    discretionary_pkgs = {
        pkg
        for pkg in migrated_pkgs
        if pkg not in target_dirs
        and pkg not in required_upstream_deps
        and not any(
            pkg.startswith(td + "/") and is_same_third_party_scope(pkg, td)
            for td in target_dirs
        )
    }
    if not discretionary_pkgs:
        return
    added_lines = 0
    for p in sorted(changed):
        norm = p.strip("/")
        if norm.startswith(ALLOWED_GLOBAL_PREFIXES) or norm.startswith(("bundles/", RUST_CRATES)):
            continue
        if is_host_os_constraints_upgrade_edit(norm):
            continue
        pkg = os.path.dirname(norm)
        if pkg and is_test_suite_only_bazel_package(pkg):
            continue
        if os.path.basename(norm) in ("BUILD.bazel", "BUILD.gn") and (
            is_exports_files_only_bazel_pkg(pkg)
            or is_collateral_existing_bazel_edit(pkg)
        ):
            continue
        added_lines += count_authored_added_lines(norm)
    anchor = f"{target_dirs[0]}/BUILD.bazel" if target_dirs else f"{sorted(migrated_pkgs)[0]}/BUILD.bazel"
    req_dep_pkgs = required_upstream_deps | {
        d for d in dependency_dirs if d.startswith("sdk/fidl/")
    }
    unassigned_pkgs = migrated_pkgs - set(target_dirs) - req_dep_pkgs
    max_dirs = max(5, len(target_dirs) + len(req_dep_pkgs))
    max_lines = max(550, 250 * (len(target_dirs) + len(req_dep_pkgs)))
    if unassigned_pkgs and (len(migrated_pkgs) > max_dirs or added_lines > max_lines):
        pkg_list_str = ", ".join(f"//{d}" for d in sorted(migrated_pkgs))
        _emit(findings, "cl_scope_exceeded", "error", anchor, 1, f"Change migrates {len(migrated_pkgs)} directories ({pkg_list_str}) and adds "
                f"{added_lines} new lines, exceeding the CL scoping limits in "
                "//build/beads/references/migration/scoping_changelists.md (at most 3-5 directories "
                "and ~500 new lines of code max).", "Reduce the scope of this CL by reverting extra co-migrated directories so that "
                "at most 5 directories and <= ~500 new lines are migrated in a single CL.")


check_cl_scoping_guidelines()

def load_platform_api_level_visibility():
    ver_path = os.path.join(workdir, "build/bazel/versioning/BUILD.bazel")
    if not os.path.isfile(ver_path):
        return None
    try:
        with open(ver_path, "r", encoding="utf-8") as f:
            ver_tree = ast.parse(f.read(), filename="build/bazel/versioning/BUILD.bazel")
    except Exception:
        return None
    var_lists = {}
    for stmt in ver_tree.body:
        if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1 and isinstance(stmt.targets[0], ast.Name):
            strs = [
                n.value
                for n in ast.walk(stmt.value)
                if isinstance(n, ast.Constant) and isinstance(n.value, str)
            ]
            var_lists[stmt.targets[0].id] = strs
    for node in ast.walk(ver_tree):
        if not isinstance(node, ast.Call):
            continue
        if not (isinstance(node.func, ast.Name) and node.func.id == "config_setting"):
            continue
        cname = ""
        vis_node = None
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                cname = kw.value.value
            elif kw.arg == "visibility":
                vis_node = kw.value
        if cname == "is_api_level_PLATFORM" and vis_node is not None:
            entries = []
            for sub in ast.walk(vis_node):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    entries.append(sub.value)
                elif isinstance(sub, ast.Name) and sub.id in var_lists:
                    entries.extend(var_lists[sub.id])
            return entries
    return None


platform_api_vis = load_platform_api_level_visibility()


def is_covered_by_platform_api_vis(pkg_dir: str) -> bool:
    if platform_api_vis is None:
        return True
    for entry in platform_api_vis:
        if entry == "//visibility:public" or entry == "//:__subpackages__":
            return True
        if entry.startswith("//") and entry.endswith(":__pkg__"):
            p = entry[2 : -len(":__pkg__")].strip("/")
            if pkg_dir == p:
                return True
        elif entry.startswith("//") and entry.endswith(":__subpackages__"):
            p = entry[2 : -len(":__subpackages__")].strip("/")
            if not p or pkg_dir == p or pkg_dir.startswith(p + "/"):
                return True
    return False


bazel_files = []
for td in dict.fromkeys(list(target_dirs) + sorted(dependency_dirs)):
    cand = os.path.join(workdir, td, "BUILD.bazel")
    if os.path.isfile(cand):
        bazel_files.append((os.path.join(td, "BUILD.bazel"), cand))


# Parse list and list-concatenation constants from sdk/atom_lists.bzl for IDK allowlist checks.
def load_sdk_atom_allowlists():
    atom_lists_path = os.path.join(workdir, "sdk/atom_lists.bzl")
    if not os.path.isfile(atom_lists_path):
        return None
    try:
        with open(atom_lists_path, "r", encoding="utf-8") as f:
            atom_tree = ast.parse(f.read(), filename="sdk/atom_lists.bzl")
    except Exception:
        return None
    atom_vars = {}

    def _eval_set(node):
        if isinstance(node, (ast.List, ast.Tuple)):
            return {
                elt.value
                for elt in node.elts
                if isinstance(elt, ast.Constant) and isinstance(elt.value, str)
            }
        if isinstance(node, ast.Name):
            return set(atom_vars.get(node.id, set()))
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            return _eval_set(node.left) | _eval_set(node.right)
        return set()

    for stmt in atom_tree.body:
        if isinstance(stmt, ast.Assign) and len(stmt.targets) == 1:
            tgt = stmt.targets[0]
            if isinstance(tgt, ast.Name):
                atom_vars[tgt.id] = _eval_set(stmt.value)
    return atom_vars


# Map host-tool package directories in sdk/BUILD.gn to their sdk_molecule target name.
def load_sdk_gn_host_tool_molecules():
    sdk_gn_path = os.path.join(workdir, "sdk/BUILD.gn")
    if not os.path.isfile(sdk_gn_path):
        return {}
    try:
        with open(sdk_gn_path, "r", encoding="utf-8") as f:
            gn_text = re.sub(r"#[^\n]*", "", f.read())
    except OSError:
        return {}
    pkg_to_mol = {}
    mol_names = (
        "build_host_tools",
        "non_build_host_tools_required_by_bazel_sdk",
        "non_build_host_tools",
    )
    for mol in mol_names:
        pat = r'\bsdk_molecule\(\s*"' + re.escape(mol) + r'"\s*\)\s*\{'
        m = re.search(pat, gn_text)
        if not m:
            continue
        depth, idx = 1, m.end()
        while idx < len(gn_text) and depth > 0:
            if gn_text[idx] == "{":
                depth += 1
            elif gn_text[idx] == "}":
                depth -= 1
            idx += 1
        body = gn_text[m.end() : idx - 1]
        for ref_pkg in re.findall(r'"//([^":\s]+)(?::[^"]+)?"', body):
            pkg_to_mol.setdefault(ref_pkg.strip("/"), mol)
    return pkg_to_mol


# Verify partner IDK macro targets in migrated BUILD.bazel files are in sdk/atom_lists.bzl.
def check_idk_atom_allowlists():
    atom_vars = load_sdk_atom_allowlists()
    if not atom_vars:
        return
    sdk_gn_mols = load_sdk_gn_host_tool_molecules()
    idk_rule_allowlists = {
        "idk_ffx_tool": ("ALL_FFX_TOOL_ATOMS", "ffx_tool"),
        "idk_host_tool": ("ALL_HOST_TOOL_ATOMS", "host_tool"),
        "idk_cc_source_library": ("ALL_CC_SOURCE_LIBRARY_ATOMS", "cc_source_library"),
        "idk_cc_source_library_zx": ("ALL_CC_SOURCE_LIBRARY_ATOMS", "cc_source_library"),
        "idk_cc_shared_library": (
            "CC_PREBUILT_SHARED_LIBRARY_ATOMS",
            "cc_prebuilt_library",
        ),
        "idk_cc_shared_library_zx": (
            "CC_PREBUILT_SHARED_LIBRARY_ATOMS",
            "cc_prebuilt_library",
        ),
        "idk_cc_static_library": (
            "CC_PREBUILT_STATIC_LIBRARY_ATOMS",
            "cc_prebuilt_library",
        ),
        "idk_cc_static_library_zx": (
            "CC_PREBUILT_STATIC_LIBRARY_ATOMS",
            "cc_prebuilt_library",
        ),
        "idk_data": ("DATA_ATOMS", "data"),
    }
    host_mol_to_list = {
        ("ffx_tool", "build_host_tools"): "BUILD_FFX_TOOLS_ATOMS",
        ("ffx_tool", "non_build_host_tools_required_by_bazel_sdk"): (
            "_NON_BUILD_FFX_TOOLS_ATOMS_REQUIRED_BY_BAZEL_SDK"
        ),
        ("ffx_tool", "non_build_host_tools"): "NON_BUILD_FFX_TOOLS_ATOMS",
        ("host_tool", "build_host_tools"): "BUILD_HOST_TOOLS_ATOMS",
        ("host_tool", "non_build_host_tools_required_by_bazel_sdk"): (
            "_NON_BUILD_HOST_TOOLS_ATOMS_REQUIRED_BY_BAZEL_SDK"
        ),
        ("host_tool", "non_build_host_tools"): "NON_BUILD_HOST_TOOLS_ATOMS",
    }
    for rel_p, full_p in bazel_files:
        pkg_dir = os.path.dirname(rel_p).strip("/")
        if pkg_dir == "build/bazel/bazel_idk/tests":
            continue
        try:
            with open(full_p, "r", encoding="utf-8") as f:
                tree = ast.parse(f.read(), filename=rel_p)
        except Exception:
            continue
        for stmt in tree.body:
            if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
                continue
            call = stmt.value
            if (
                not isinstance(call.func, ast.Name)
                or call.func.id not in idk_rule_allowlists
            ):
                continue
            rule_name = call.func.id
            kws = {kw.arg: kw.value for kw in call.keywords if kw.arg}
            nm_node = kws.get("name")
            cat_node = kws.get("category")
            if not (isinstance(nm_node, ast.Constant) and isinstance(nm_node.value, str)):
                continue
            if not (isinstance(cat_node, ast.Constant) and cat_node.value == "partner"):
                continue
            list_var, atom_type = idk_rule_allowlists[rule_name]
            if list_var not in atom_vars:
                continue
            atom_label = f"//{pkg_dir}:{nm_node.value}_idk"
            if atom_label in atom_vars[list_var]:
                continue
            mol = sdk_gn_mols.get(pkg_dir, "")
            exact_list = host_mol_to_list.get((atom_type, mol), "")
            if exact_list:
                target_list_desc = f"`{exact_list}` (matching `{mol}` in `sdk/BUILD.gn`)"
            elif atom_type == "ffx_tool":
                target_list_desc = (
                    "`BUILD_FFX_TOOLS_ATOMS`, "
                    "`_NON_BUILD_FFX_TOOLS_ATOMS_REQUIRED_BY_BAZEL_SDK`, or "
                    "`NON_BUILD_FFX_TOOLS_ATOMS` (matching its `sdk_molecule` in `sdk/BUILD.gn`)"
                )
            elif atom_type == "host_tool":
                target_list_desc = (
                    "`BUILD_HOST_TOOLS_ATOMS`, "
                    "`_NON_BUILD_HOST_TOOLS_ATOMS_REQUIRED_BY_BAZEL_SDK`, or "
                    "`NON_BUILD_HOST_TOOLS_ATOMS` (matching its `sdk_molecule` in `sdk/BUILD.gn`)"
                )
            elif atom_type == "cc_source_library":
                stable_node = kws.get("stable")
                is_unstable = (
                    isinstance(stable_node, ast.Constant) and stable_node.value is False
                )
                target_list_desc = (
                    "`UNSTABLE_CC_SOURCE_LIBRARY_ATOMS`"
                    if is_unstable
                    else "`STABLE_CC_SOURCE_LIBRARY_ATOMS`"
                )
            else:
                target_list_desc = f"`{list_var}`"
            msg = (
                f'`{rule_name}(name = "{nm_node.value}", category = "partner")` '
                f"generates IDK atom `{atom_label}`, which is missing from `{list_var}` "
                "in `sdk/atom_lists.bzl` (`verify_target_is_in_allowlist` fails loading)."
            )
            rem = (
                f'Add `"{atom_label}"` (kept sorted) to {target_list_desc} in '
                "`sdk/atom_lists.bzl`."
            )
            _emit(
                findings,
                "missing_idk_atom_allowlist",
                "error",
                rel_p,
                stmt.lineno,
                msg,
                rem,
            )


check_idk_atom_allowlists()

for rel_path, full_path in bazel_files:
    try:
        with open(full_path, "r", encoding="utf-8") as f:
            source = f.read()
        tree = ast.parse(source, filename=rel_path)
    except Exception:
        continue
    pkg_dir = os.path.dirname(rel_path)
    # Whitespace-free copy of the file and per-target call slices at the change base:
    # pre-existing targets, attributes, and @bazel2gn annotations predate the change.
    base_bazel_text = "\n".join(git_lines(["show", f"{change_base}:{rel_path}"]))
    base_bazel_flat = re.sub(r"\s+", "", base_bazel_text)
    base_target_flats = {}
    base_redundant_lint_targets = set()
    if base_bazel_text:
        try:
            base_tree = ast.parse(base_bazel_text, filename=rel_path)
            for b_node in ast.walk(base_tree):
                if not isinstance(b_node, ast.Call):
                    continue
                b_rule = (
                    b_node.func.id
                    if isinstance(b_node.func, ast.Name)
                    else (b_node.func.attr if isinstance(b_node.func, ast.Attribute) else "")
                )
                for b_kw in b_node.keywords:
                    if (
                        b_kw.arg == "name"
                        and isinstance(b_kw.value, ast.Constant)
                        and isinstance(b_kw.value.value, str)
                    ):
                        b_seg = ast.get_source_segment(base_bazel_text, b_node) or ""
                        base_target_flats[(b_rule, b_kw.value.value)] = re.sub(
                            r"\s+", "", b_seg
                        )
                        if _redundant_rust_lint_config_info(b_node, b_rule, pkg_dir) is not None:
                            base_redundant_lint_targets.add((b_rule, b_kw.value.value))
        except Exception:
            pass

    def is_preexisting_target_attr(r_name: str, t_name: str, attr_snippet: str = "") -> bool:
        """True when target (r_name, t_name) and optional attr_snippet existed at change_base."""
        b_flat = base_target_flats.get((r_name, t_name))
        if b_flat is None:
            return False
        if not attr_snippet:
            return True
        return re.sub(r"\s+", "", attr_snippet) in b_flat

    base_gn_lines = git_lines(["show", f"{change_base}:{pkg_dir}/BUILD.gn"])
    base_gn_text = "\n".join(base_gn_lines)
    src_lines = source.splitlines()

    # Collect in-package rustc_library targets (mapping name -> normalized crate_name)
    # so unit-test rustc_test targets can be checked for `crate = ":<lib>"` reuse and
    # valid bazel2gn test output_name disambiguation.
    pkg_rustc_libs = {}
    for stmt_node in ast.walk(tree):
        if isinstance(stmt_node, ast.Call) and isinstance(stmt_node.func, ast.Name):
            if stmt_node.func.id in (
                "rustc_library",
                "rustc_proc_macro",
                "rustc_cdylib",
                "fuchsia_rust_dso",
            ):
                l_name, l_crate = "", ""
                for kw in stmt_node.keywords:
                    if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        l_name = kw.value.value
                    elif kw.arg == "crate_name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        l_crate = kw.value.value
                if l_name:
                    pkg_rustc_libs[l_name] = (l_crate or l_name).replace("-", "_")

    # Extract non-directive comment lines (with 1-based line numbers) preceding a target call.
    def preceding_comment_lines(lineno: int):
        comments = []
        idx = lineno - 2
        while idx >= 0:
            s_line = src_lines[idx].strip()
            if not s_line:
                idx -= 1
                continue
            if s_line.startswith("#"):
                if "@bazel2gn:" not in s_line:
                    comments.append((idx + 1, s_line.lower()))
                idx -= 1
                continue
            break
        return list(reversed(comments))

    # Check that unit-test rustc_test targets use `crate = ":<lib>"` and omit `crate_name`,
    # except when GN `non_test_deps` swaps a dependency for a `testutils` variant (in which
    # case a standalone `rustc_test` with a concise justifying comment above it is required).
    def check_rustc_test_crate_reuse(call_node, r_name, t_name, k_map):
        """Verify unit-test rustc_test targets use `crate = ":<lib>"` or concisely document standalone testutils deps."""
        if r_name != "rustc_test" or not t_name:
            return
        kw_names = {kw.arg for kw in call_node.keywords if kw.arg}
        lib_stem = t_name[:-len("_test")] if t_name.endswith("_test") else ""
        has_matching_lib = bool(lib_stem and lib_stem in pkg_rustc_libs)
        if (
            "srcs" in kw_names
            and has_matching_lib
            and "crate" not in kw_names
            and not is_preexisting_target_attr(r_name, t_name, "srcs=")
        ):
            srcs_line = next(
                (kw.lineno for kw in call_node.keywords if kw.arg == "srcs"),
                getattr(call_node, "lineno", 1),
            )
            deps_kw = next((kw.value for kw in call_node.keywords if kw.arg == "deps"), None)
            deps_seg = (ast.get_source_segment(source, deps_kw) or "").lower() if deps_kw else ""
            needs_standalone = ("non_test_deps" in base_gn_text) or ("testutils" in deps_seg)
            comment_entries = preceding_comment_lines(getattr(call_node, "lineno", 1))
            comment_txt = "\n".join(txt for _, txt in comment_entries)
            has_justification = any(w in comment_txt for w in ("crate", "testutils"))
            verbose_line = next(
                (
                    ln
                    for ln, txt in reversed(comment_entries)
                    if "non_test_deps" in txt or "inherits" in txt
                ),
                None,
            )
            if needs_standalone:
                if not has_justification:
                    _emit(findings, "rustc_test_missing_standalone_comment", "error", rel_path, srcs_line, f"Target '{t_name}' (rustc_test) is not attached to `:{lib_stem}` via "
                            "`crate` (because unit tests require a `testutils` dependency variant), "
                            "but lacks a comment above `rustc_test` justifying why `crate` cannot "
                            "be used.", f"Add a concise comment immediately above `{t_name}` in '{rel_path}' "
                            f"explaining why it cannot attach to `:{lib_stem}` via `crate` "
                            "(because unit tests require the `testutils` dependency variant), "
                            "without adding boilerplate about `rustc_test(crate = ...)` inheriting "
                            "`deps` or GN `non_test_deps`.")
                elif verbose_line is not None:
                    _emit(findings, "rustc_test_verbose_standalone_comment", "error", rel_path, verbose_line, f"Comment above standalone '{t_name}' (rustc_test) includes "
                            "unnecessary rule-mechanics or GN-comparison boilerplate "
                            "(`rustc_test(crate = ...)` inheriting `deps` / GN `non_test_deps`).", f"Trim the comment above `{t_name}` in '{rel_path}' so it states only "
                            f"the target-specific reason `crate = \":{lib_stem}\"` cannot be used "
                            "(that unit tests require the `testutils` dependency variant), and "
                            "remove the clause about Bazel `rustc_test(crate = ...)` inheriting "
                            "`deps` or GN `non_test_deps`.")
            else:
                _emit(findings, "rustc_test_repeats_library_srcs", "error", rel_path, srcs_line, f"Target '{t_name}' (rustc_test) repeats `srcs` (and library features/deps) "
                        f"instead of referencing sibling library `:{lib_stem}` via `crate` "
                        "(`bazel2gn` natively supports `crate = \":<lib>\"` on `rustc_test` and "
                        "inlines the crate's `srcs`, `edition`, `deps`, and `proc_macro_deps` into "
                        "the generated GN `rustc_test`).", f"Set `crate = \":{lib_stem}\"` on '{t_name}' and remove `srcs`, "
                        "`edition`, `crate_name`, any `deps`/`proc_macro_deps`/`crate_features` "
                        f"already inherited from `:{lib_stem}`, any single-use `_<LIB>_*` "
                        "variables, and any comment claiming `bazel2gn` requires `srcs`/`deps` "
                        f"on `rustc_test`, then re-run `fx bazel2gn -d {pkg_dir}`.")
        if "crate_name" in k_map and ("crate" in kw_names or has_matching_lib):
            c_name, c_line = k_map["crate_name"]
            gn_path = os.path.join(workdir, pkg_dir, "BUILD.gn")
            has_gn_sentinel = os.path.isfile(gn_path) and "BAZEL2GN SENTINEL" in open(
                gn_path, encoding="utf-8"
            ).read()
            norm_t = t_name.replace("-", "_")
            norm_c = c_name.replace("-", "_")
            crate_kw = next(
                (
                    kw.value.value.lstrip(":")
                    for kw in call_node.keywords
                    if kw.arg == "crate"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ),
                lib_stem,
            )
            lib_crate_name = pkg_rustc_libs.get(crate_kw, "")
            base_gn_norm = base_gn_text.replace("-", "_")
            # bazel2gn maps rustc_test `crate_name` to GN `output_name`. Allow a non-default
            # test crate_name (distinct from the library crate_name itself) when preserving a
            # base BUILD.gn custom test name/output_name or disambiguating the GN test binary
            # in a dual-build package.
            valid_gn_test_output_name = (
                norm_c != norm_t
                and norm_c != lib_crate_name
                and (
                    f'"{norm_c}"' in base_gn_norm
                    or (
                        has_gn_sentinel
                        and norm_c.endswith(
                            ("_test", "_tests", "_unittests", "_lib_test", "_bin_test")
                        )
                    )
                )
            )
            if not valid_gn_test_output_name and not is_preexisting_target_attr(r_name, t_name, f'crate_name="{c_name}"'):
                _emit(findings, "redundant_default_attribute", "error", rel_path, c_line, f"Target '{t_name}' (rustc_test) specifies `crate_name = \"{c_name}\"`, "
"which is unnecessary on a crate unit test (unless setting a distinct "
                        "`<crate>_test` binary `output_name` for `bazel2gn`).", f"Remove `crate_name` from `{t_name}` in '{rel_path}' (or set it to "
                        f"`\"{lib_crate_name or norm_t}_test\"` only when `bazel2gn` must emit a "
                        "disambiguated GN `output_name`).")
        elif "crate_name" in k_map:
            # A standalone rustc_test (no `crate`, no sibling library) already gets
            # name.replace("-", "_") as its crate and GN output name in both builds.
            c_name, c_line = k_map["crate_name"]
            if c_name.replace("-", "_") == t_name.replace("-", "_") and not is_preexisting_target_attr(
                r_name, t_name, f'crate_name="{c_name}"'
            ):
                _emit(findings, "redundant_default_attribute", "error", rel_path, c_line, f"Target '{t_name}' (rustc_test) specifies `crate_name = \"{c_name}\"`, which is "
                        "already the default derived from `name` (with `-` replaced by `_`) in both rules_rust "
                        "and GN, so bazel2gn only emits a redundant `output_name`.", f"Remove `crate_name` from `{t_name}` in '{rel_path}' and re-run "
                        f"`fx bazel2gn -d {pkg_dir}`.")
        if "crate" in kw_names and "edition" in k_map and not is_preexisting_target_attr(r_name, t_name, "edition="):
            ed_val, ed_line = k_map["edition"]
            _emit(findings, "redundant_default_attribute", "error", rel_path, ed_line, f"Target '{t_name}' (rustc_test) sets `edition = \"{ed_val}\"` next to `crate`, but both "
                    "rules_rust and bazel2gn take `edition` from the crate and ignore it on the test.", f"Remove `edition` from `{t_name}` in '{rel_path}' (bazel2gn output does not change).")
        if (
            "crate" in kw_names
            and "crate_name" in k_map
            and not preceding_comment_lines(k_map["crate_name"][1])
            and not is_preexisting_target_attr(r_name, t_name, f'crate_name="{k_map["crate_name"][0]}"')
        ):
            c_name, c_line = k_map["crate_name"]
            _emit(findings, "rustc_test_crate_name_missing_comment", "warning", rel_path, c_line, f"Target '{t_name}' (rustc_test) sets `crate_name = \"{c_name}\"` next to `crate`. "
                    "rules_rust ignores it there; it only sets the GN `output_name` bazel2gn emits, which "
                    "readers can't tell from BUILD.bazel.", f"Add a short comment above `crate_name` in `{t_name}` saying what needs the GN binary "
                    f"name `{c_name}` (e.g. the `.cml` `program.binary`, or avoiding a `lib_test` collision).")

    # Check that newly added rustc_test / rust_test / host_rustc_test targets do not use
    # `# @bazel2gn:raw_attribute:output_name = "<out_name>"`: naming the test target
    # itself `name = "<out_name>"` gives GN `bin/<out_name>` by default without an
    # annotation while `wrap_host_rust_test` preserves its `tests.json` target name.
    # Pre-existing test targets at change_base are exempt so modifying a dependency's
    # BUILD.bazel (e.g. widening visibility) does not demand renaming existing targets
    # (which would violate migration_sanity/dropped_preexisting_bazel_target).
    def check_rustc_test_raw_output_name(call_node, r_name, t_name):
        """Flag `# @bazel2gn:raw_attribute:output_name` on newly added Rust test and C++ binary/test targets."""
        is_rust_test = r_name in ("rustc_test", "rust_test", "host_rustc_test")
        is_cc_bin = r_name in ("fx_cc_binary", "fx_cc_test_binary", "fx_cc_test")
        if (not is_rust_test and not is_cc_bin) or not t_name:
            return
        idx = getattr(call_node, "lineno", 1) - 2
        raw_out_re = re.compile(
            r'#\s*@bazel2gn:raw_attribute:\s*output_name\s*=\s*"([^"]+)"'
        )
        while idx >= 0:
            s_line = src_lines[idx].strip()
            if not s_line:
                idx -= 1
                continue
            if s_line.startswith("#"):
                m = raw_out_re.search(s_line)
                if m:
                    if is_preexisting_target_attr(r_name, t_name) and (
                        re.sub(r"\s+", "", s_line) in base_bazel_flat
                    ):
                        idx -= 1
                        continue
                    out_name = m.group(1)
                    annot_line = idx + 1
                    if is_cc_bin:
                        msg_text = (
                            f"Target '{t_name}' ({r_name}) uses "
                            f"`# @bazel2gn:raw_attribute:output_name = \"{out_name}\"` "
                            f"instead of the native `gn_output_name = \"{out_name}\"` attribute."
                        )
                        rem_text = (
                            f"Remove `# @bazel2gn:raw_attribute:output_name = \"{out_name}\"` "
                            f"(and any comment justifying it) above `{t_name}` in '{rel_path}', "
                            f"set `gn_output_name = \"{out_name}\"` on `{r_name}(name = \"{t_name}\")` "
                            f"(omit if `{out_name}` equals `{t_name}`), and re-run "
                            f"`fx bazel2gn -d {pkg_dir}`."
                        )
                        _emit(findings, "cc_binary_raw_output_name_annotation", "error", rel_path, annot_line, msg_text, rem_text)
                    else:
                        msg_text = (
                            f"Target '{t_name}' ({r_name}) uses "
                            f"`# @bazel2gn:raw_attribute:output_name = \"{out_name}\"`, "
                            f"which is unnecessary when the test target is named "
                            f"`\"{out_name}\"` (giving GN `bin/{out_name}` by default to "
                            "match any `.cml` manifest while `wrap_host_rust_test` keeps "
                            "its `tests.json` name)."
                        )
                        rem_text = (
                            f"Remove `# @bazel2gn:raw_attribute:output_name = \"{out_name}\"` "
                            f"above `{t_name}` in '{rel_path}', rename the `{r_name}` target "
                            f"to `name = \"{out_name}\"`, update any in-package test wrappers "
                            f"(`binary_name = \"{out_name}\"` on `wrap_host_rust_test` while "
                            f"keeping its `name`, `binary = \":{out_name}\"` on "
                            f"`fx_packaged_binary`, or `unit_tests = [\":{out_name}\"]` on "
                            f"`fx_unittest_package` / `fx_unittest_component`), re-run "
                            f"`fx bazel2gn -d {pkg_dir}`, and update any hand-written "
                            f"`{pkg_dir}/BUILD.gn` references (`\":{t_name}\"` and "
                            f"`\":{t_name}($host_toolchain)\"`) to `\":{out_name}\"` and "
                            f"`\":{out_name}($host_toolchain)\"`."
                        )
                        _emit(findings, "rustc_test_raw_output_name_annotation", "error", rel_path, annot_line, msg_text, rem_text)
                idx -= 1
                continue
            break


    # Flag TODO comments missing the required fxbug.dev link format.
    for idx_l, raw_l in enumerate(src_lines, start=1):
        if re.search(r"^\s*#.*?\bTODO\b", raw_l):
            if "https://fxbug.dev/" not in raw_l:
                _emit(
                    findings,
                    "invalid_todo_format",
                    "error",
                    rel_path,
                    idx_l,
                    f"Line contains a TODO comment without the required tracking bug format.",
                    f"Update the TODO in '{rel_path}' to use the `TODO(https://fxbug.dev/<id>)` format.",
                )

    # Flag WLAN-only tracking bug 543568916 when cited in non-WLAN packages.
    if pkg_dir and not pkg_dir.startswith("src/connectivity/wlan"):
        for idx_l, raw_l in enumerate(src_lines, start=1):
            if "543568916" in raw_l and re.sub(r"\s+", "", raw_l) not in base_bazel_flat:
                _emit(
                    findings,
                    "wrong_subsystem_todo_bug",
                    "error",
                    rel_path,
                    idx_l,
                    f"Line cites `543568916` (the WLAN-only Bazel migration tracking bug) in non-WLAN package `//{pkg_dir}`.",
                    f"Replace `543568916` in '{rel_path}' with `570694755` (general Bazel migration tech debt) or a verified open bug for `//{pkg_dir}`.",
                )

    # Check that newly added top-level _-prefixed variables are shared across >= 2 references.
    def check_single_use_private_vars(tree_node):
        """Flag newly added top-level _-prefixed variables referenced fewer than 2 times."""
        load_counts = {}
        for n in ast.walk(tree_node):
            if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Load):
                load_counts[n.id] = load_counts.get(n.id, 0) + 1
        for stmt in tree_node.body:
            if not (
                isinstance(stmt, ast.Assign)
                and len(stmt.targets) == 1
                and isinstance(stmt.targets[0], ast.Name)
            ):
                continue
            v_name = stmt.targets[0].id
            if not v_name.startswith("_"):
                continue
            seg = ast.get_source_segment(source, stmt) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            uses = load_counts.get(v_name, 0)
            if uses < 2:
                _emit(findings, "single_use_private_variable", "error", rel_path, stmt.lineno, f"Top-level private variable `{v_name}` in '{rel_path}' is referenced "
                        f"{uses} time(s); shared `_` variables should only be introduced when "
                        "reused across multiple targets.", f"Inline `{v_name}` directly into its target in '{rel_path}' (or remove "
                        f"it if unused) and re-run `fx bazel2gn -d {pkg_dir}` if dual-building.")

    # Check wrap_host_rust_test naming in rustc_library_with_features packages with a single host test.
    def check_wrap_host_rust_test_naming(tree_node, gn_text):
        """Verify wrap_host_rust_test is named <base>-host-test for rustc_library_with_features."""
        feat_m = re.search(r'\brustc_library_with_features\(\s*"([^"]+)"\s*\)', gn_text)
        if not feat_m:
            return
        base_lib = feat_m.group(1)
        if "-" not in base_lib:
            return
        host_wrappers = []
        for stmt_node in ast.walk(tree_node):
            if (
                isinstance(stmt_node, ast.Call)
                and isinstance(stmt_node.func, ast.Name)
                and stmt_node.func.id == "wrap_host_rust_test"
            ):
                w_name, w_bin, w_line = "", "", getattr(stmt_node, "lineno", 1)
                for kw in stmt_node.keywords:
                    if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        w_name = kw.value.value
                        w_line = kw.lineno
                    elif kw.arg == "binary_name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                        w_bin = kw.value.value
                if w_name:
                    host_wrappers.append((w_name, w_bin, w_line, stmt_node))
        if len(host_wrappers) != 1:
            return
        w_name, w_bin, w_line, w_node = host_wrappers[0]
        seg = ast.get_source_segment(source, w_node) or ""
        if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
            return
        expected_name = f"{base_lib}-host-test"
        if w_bin in (f"{base_lib}-instrumented_test", f"{base_lib}_test") and w_name != expected_name:
            _emit(findings, "wrap_host_rust_test_naming", "error", rel_path, w_line, f"Target '{w_name}' (wrap_host_rust_test) wraps `{w_bin}` for "
                    f"`rustc_library_with_features(\"{base_lib}\")`, which should be named "
                    f"`\"{expected_name}\"` instead of including the variant suffix or `_host_test`.", f"Rename `{w_name}` to `\"{expected_name}\"` in '{rel_path}' and update its "
                    f"`host_tests` reference in `{pkg_dir}/BUILD.gn`.")

    # Flag deprecated with_unit_tests / with_host_unit_tests / test_deps on rustc_* outside
    # //build/bazel/rules/rust/generate_unit_tests.bzl _ALLOWED_PACKAGES.
    def check_disallowed_rustc_unit_test_attrs(tree_node):
        gen_ut_path = os.path.join(
            workdir, "build/bazel/rules/rust/generate_unit_tests.bzl"
        )
        if not os.path.isfile(gen_ut_path):
            return
        try:
            gen_ut_txt = open(gen_ut_path, encoding="utf-8").read()
        except OSError:
            return
        if "_ALLOWED_PACKAGES" not in gen_ut_txt or f'"{pkg_dir}":' in gen_ut_txt:
            return
        for stmt_node in ast.walk(tree_node):
            if not (
                isinstance(stmt_node, ast.Call)
                and isinstance(stmt_node.func, ast.Name)
                and stmt_node.func.id
                in ("rustc_library", "rustc_binary", "rustc_proc_macro")
            ):
                continue
            r_id = stmt_node.func.id
            t_name = ""
            bad_attrs = []
            bad_line = getattr(stmt_node, "lineno", 1)
            for kw in stmt_node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    t_name = kw.value.value
                elif kw.arg in ("with_unit_tests", "with_host_unit_tests"):
                    if not (
                        isinstance(kw.value, ast.Constant)
                        and kw.value.value in (False, None)
                    ):
                        bad_attrs.append(kw.arg)
                        bad_line = getattr(kw, "lineno", bad_line)
                elif kw.arg == "test_deps":
                    if not (isinstance(kw.value, ast.List) and not kw.value.elts):
                        bad_attrs.append(kw.arg)
                        bad_line = getattr(kw, "lineno", bad_line)
            if bad_attrs:
                attrs_str = ", ".join(f"`{a}`" for a in bad_attrs)
                _emit(
                    findings,
                    "deprecated_rustc_with_unit_tests",
                    "error",
                    rel_path,
                    bad_line,
                    f"`{r_id}(\"{t_name}\")` in '{rel_path}' sets {attrs_str}, which fails "
                    "Bazel evaluation (`_check_unit_test_allowlist` in "
                    "'build/bazel/rules/rust/generate_unit_tests.bzl') because "
                    f"`{pkg_dir}` is not in `_ALLOWED_PACKAGES`.",
                    f"Remove {attrs_str} from `{r_id}(\"{t_name}\")` in '{rel_path}' and "
                    f"declare a standalone `rustc_test(name = \"{t_name}_test\", "
                    f"crate = \":{t_name}\", deps = [...])` or `host_rustc_test(name = "
                    f"\"{t_name}_test\", crate = \":{t_name}\", deps = [...])` target "
                    "instead (setting `crate_name = \"<crate>_lib_test\"` when `name` is "
                    "generic like `lib_test`).",
                )

    # Check for rustc_check_cfg_build_flags(cfgs = ["test"]) in packages with no tests.
    def check_unnecessary_test_check_cfg_flags(tree_node):
        """Flag rustc_check_cfg_build_flags(cfgs = ["test"]) when the package has no tests."""
        check_disallowed_rustc_unit_test_attrs(tree_node)
        has_pkg_tests = False
        for stmt_node in ast.walk(tree_node):
            if not (isinstance(stmt_node, ast.Call) and isinstance(stmt_node.func, ast.Name)):
                continue
            r_id = stmt_node.func.id
            if r_id in ("rustc_test", "rust_test", "fx_unittest_package", "fx_test"):
                has_pkg_tests = True
                break
            if r_id.startswith(("rustc_", "rust_")):
                for kw in stmt_node.keywords:
                    if kw.arg in ("with_unit_tests", "with_host_unit_tests") and isinstance(kw.value, ast.Constant):
                        if kw.value.value is True or kw.value.value in ("host", "fuchsia", "both"):
                            has_pkg_tests = True
                            break
        if has_pkg_tests:
            return
        pkg_abs = os.path.join(workdir, pkg_dir)
        for root, _, files in os.walk(pkg_abs):
            for fn in files:
                if fn.endswith(".rs"):
                    try:
                        rs_txt = open(os.path.join(root, fn), encoding="utf-8").read()
                        if re.search(r"#\s*\[\s*(?:cfg|cfg_attr)\s*\([^]]*\btest\b", rs_txt):
                            return
                    except OSError:
                        pass
        for stmt_node in ast.walk(tree_node):
            if not (
                isinstance(stmt_node, ast.Call)
                and isinstance(stmt_node.func, ast.Name)
                and stmt_node.func.id == "rustc_check_cfg_build_flags"
            ):
                continue
            seg = ast.get_source_segment(source, stmt_node) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            cfg_name = ""
            cfgs_list, feats_list = [], []
            for kw in stmt_node.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    cfg_name = kw.value.value
                elif kw.arg == "cfgs" and isinstance(kw.value, ast.List):
                    cfgs_list = [
                        e.value for e in kw.value.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)
                    ]
                elif kw.arg == "features" and isinstance(kw.value, ast.List):
                    feats_list = [
                        e.value for e in kw.value.elts if isinstance(e, ast.Constant) and isinstance(e.value, str)
                    ]
            if cfgs_list == ["test"] and not feats_list:
                _emit(findings, "unnecessary_test_check_cfg_flags", "error", rel_path, getattr(stmt_node, "lineno", 1), f"`rustc_check_cfg_build_flags(name = \"{cfg_name}\", cfgs = [\"test\"])` in "
                        f"'{rel_path}' checks only `cfg(test)`, but this package has no unit tests or "
                        "`#[cfg(test)]` items.", f"Remove `rustc_check_cfg_build_flags(name = \"{cfg_name}\", ...)`, its "
                        f"`\":{cfg_name}\"` reference in `build_flags`, and any unused `load(...)` from "
                        f"'{rel_path}', then re-run `fx bazel2gn -d {pkg_dir}`.")

    # Check that select({...}) blocks in deps repeated across >= 2 targets are extracted into _COMMON_DEPS.
    def check_repeated_deps_select(tree_node):
        """Flag select({...}) expressions in target deps repeated across multiple targets."""
        select_occurrences = {}
        for stmt_node in ast.walk(tree_node):
            if not (isinstance(stmt_node, ast.Call) and isinstance(stmt_node.func, ast.Name)):
                continue
            for kw in stmt_node.keywords:
                if kw.arg != "deps":
                    continue
                for sub in ast.walk(kw.value):
                    if (
                        isinstance(sub, ast.Call)
                        and isinstance(sub.func, ast.Name)
                        and sub.func.id == "select"
                    ):
                        seg = ast.get_source_segment(source, sub) or ""
                        norm_seg = re.sub(r"\s+", "", seg)
                        if norm_seg and norm_seg not in base_bazel_flat:
                            select_occurrences.setdefault(norm_seg, []).append(getattr(sub, "lineno", kw.lineno))
        for _norm_seg, lines_list in select_occurrences.items():
            if len(lines_list) >= 2:
                _emit(findings, "repeated_common_deps_select", "error", rel_path, lines_list[0], f"Identical `select({{...}})` block in `deps` is repeated across {len(lines_list)} "
                        f"targets in '{rel_path}' instead of being extracted into `_COMMON_DEPS`.", f"Extract the shared `select({{...}})` dependency block into top-level "
                        "`_COMMON_DEPS` (annotated with `# @bazel2gn:transformer=deps` immediately above "
                        "`_COMMON_DEPS = ...` when dual-building with `bazel2gn`), reference `_COMMON_DEPS` "
                        f"in each target's `deps`, and re-run `fx bazel2gn -d {pkg_dir}`.")

    # Check for redundant single-target forwarding alias(name = "<outer>", actual = ":<inner>")
    # or unfolded `name = "lib"` with `crate_name = "<outer>"` when `:<inner>` has no external
    # callers and its crate/output name already matches `<outer>`.
    def check_redundant_forwarding_alias(tree_node, gn_text):
        """Flag redundant `alias(name = "<outer>", actual = ":<inner>")` when `:<inner>` can be folded into `<outer>`."""
        if not gn_text:
            return
        aliases = {}
        libs = {}
        for stmt_node in tree_node.body:
            if not (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
            ):
                continue
            call_n = stmt_node.value
            r_id = call_n.func.id
            seg = ast.get_source_segment(source, stmt_node) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            k_strs = {
                kw.arg: kw.value.value
                for kw in call_n.keywords
                if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str)
            }
            nm = k_strs.get("name", "")
            if not nm:
                continue
            if r_id == "alias" and k_strs.get("actual", "").startswith(":"):
                aliases[nm] = (k_strs["actual"][1:], getattr(call_n, "lineno", 1))
            elif r_id in (
                "rustc_library",
                "rust_library",
                "rustc_proc_macro",
                "rust_proc_macro",
                "rustc_dylib",
                "fx_cc_library",
                "cc_library",
                "fx_cc_library_headers",
            ):
                libs[nm] = (r_id, k_strs, getattr(call_n, "lineno", 1))
        candidates = dict(aliases)
        if "lib" in libs:
            _, lib_k, lib_line = libs["lib"]
            lib_crate = lib_k.get("crate_name", "")
            if lib_crate and lib_crate != "lib" and lib_crate not in candidates and lib_crate not in libs:
                candidates[lib_crate] = ("lib", lib_line)
        if not candidates:
            return
        for outer, (inner, alias_line) in sorted(candidates.items()):
            if inner == outer or inner not in libs:
                continue
            r_id, k_strs, _ = libs[inner]
            if r_id.startswith(("rustc_", "rust_")):
                eff_crate = k_strs.get("crate_name", inner)
                if eff_crate != outer and eff_crate != outer.replace("-", "_"):
                    continue
            elif inner != "lib" or outer != os.path.basename(pkg_dir):
                continue
            grp_m = re.search(
                r'\bgroup\(\s*"' + re.escape(outer) + r'"\s*\)\s*\{([^}]*)\}', gn_text
            )
            if not grp_m:
                continue
            grp_body = grp_m.group(1)
            grp_deps = re.findall(r'"([^"]+)"', grp_body)
            if grp_deps not in ([f":{inner}"], [f"//{pkg_dir}:{inner}"]):
                continue
            other_refs = re.findall(
                r'":' + re.escape(inner) + r'"|"//' + re.escape(pkg_dir) + r":" + re.escape(inner) + r'"',
                source,
            )
            if len(other_refs) > 2:
                continue
            try:
                ext_out = subprocess.check_output(
                    [
                        "git",
                        "-C",
                        workdir,
                        "grep",
                        "-l",
                        "-F",
                        f"//{pkg_dir}:{inner}",
                        "--",
                        "BUILD.gn",
                        "BUILD.bazel",
                        "*.gni",
                        "*.bzl",
                        "**/BUILD.gn",
                        "**/BUILD.bazel",
                    ],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
            except Exception:
                ext_out = ""
            ext_files = [
                f.strip().strip("/")
                for f in ext_out.splitlines()
                if f.strip().strip("/")
                and not (
                    f.strip().strip("/") == pkg_dir
                    or f.strip().strip("/").startswith(pkg_dir + "/")
                )
            ]
            if ext_files:
                continue
            _emit(findings, "redundant_forwarding_alias", "error", rel_path, alias_line, f"Target '{outer}' (`alias` to `:{inner}`) and '{inner}' (`{r_id}`) in "
                    f"'{rel_path}' preserve a redundant single-target forwarding indirection "
                    f"when no external caller depends on `//{pkg_dir}:{inner}`.", f"Fold `:{inner}` directly into `{r_id}(name = \"{outer}\", ...)` in "
                    f"'{rel_path}' (removing `alias(name = \"{outer}\")`, omitting `crate_name` "
                    f"when it matches `{outer}`, updating any same-package unit test "
                    f"`crate = \":{outer}\"`, and removing `group(\"{outer}\")` from "
                    f"'{pkg_dir}/BUILD.gn'), then re-run `fx bazel2gn -d {pkg_dir}`.")

    # Check for commented-out dependency or source entries carried over into BUILD.bazel lists.
    def check_commented_out_deps(tree_node):
        """Flag commented-out dependency or source labels in BUILD.bazel list attributes."""
        commented_item_re = re.compile(
            r'^\s*#\s*["\']((?://|:|@)[^"\'\s]+|[A-Za-z0-9_./-]+\.'
            r'(?:rs|cc|cpp|c|h|hpp|py|go|fidl|cml|json\d*))["\']\s*,?\s*(?:#.*)?$'
        )
        list_attrs = {
            "deps",
            "test_deps",
            "implementation_deps",
            "link_deps",
            "proc_macro_deps",
            "srcs",
            "hdrs",
            "data",
            "test_data",
            "public_deps",
            "runtime_deps",
            "compile_data",
        }

        def scan_span(
            start_ln: int,
            end_ln: int,
            owner_desc: str,
            attr_desc: str,
        ):
            items = []
            for ln in range(max(1, start_ln), min(len(src_lines), end_ln) + 1):
                m = commented_item_re.match(src_lines[ln - 1])
                if m:
                    items.append((ln, m.group(1)))
            if not items:
                return
            first_ln = items[0][0]
            quoted_items = ", ".join(f'`"{lbl}"`' for _, lbl in items)
            line_nums = ", ".join(str(ln) for ln, _ in items)
            _emit(findings, "commented_out_dependency", "error", rel_path, first_ln, f"{owner_desc} in '{rel_path}' contains commented-out {attr_desc} "
                    f"({quoted_items} on line(s) {line_nums}); dead commented-out "
                    "dependencies or sources must be removed rather than carried over "
                    "to BUILD.bazel.", f"Delete the commented-out line(s) ({line_nums}) from {attr_desc} "
                    f"in '{rel_path}' (and re-run `fx bazel2gn -d {pkg_dir}` if "
                    "dual-building).")

        for stmt_node in tree_node.body:
            if (
                isinstance(stmt_node, ast.Assign)
                and len(stmt_node.targets) == 1
                and isinstance(stmt_node.targets[0], ast.Name)
            ):
                v_name = stmt_node.targets[0].id
                scan_span(
                    getattr(stmt_node, "lineno", 1),
                    getattr(stmt_node, "end_lineno", getattr(stmt_node, "lineno", 1)),
                    f"Top-level list `{v_name}`",
                    f"`{v_name}` entries",
                )
            elif (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
            ):
                call_n = stmt_node.value
                r_id = call_n.func.id
                t_name = ""
                for kw in call_n.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        t_name = kw.value.value
                owner = f"Target '{t_name}' ({r_id})" if t_name else f"`{r_id}(...)`"
                for kw in call_n.keywords:
                    if kw.arg in list_attrs:
                        start_l = getattr(kw, "lineno", getattr(kw.value, "lineno", 1))
                        end_l = getattr(kw, "end_lineno", getattr(kw.value, "end_lineno", start_l))
                        scan_span(
                            start_l,
                            end_l,
                            owner,
                            f"`{kw.arg}` entries",
                        )

    # Inspect workspace-loaded .bzl macros that set host platform constraints internally
    # and do not accept `target_compatible_with` at call sites.
    def resolve_workspace_forbid_tcw_macros(tree_node):
        """Return the forbid_tcw_macros loaded from workspace .bzl files."""
        forbid_tcw = set()
        for stmt_n in tree_node.body:
            if not (
                isinstance(stmt_n, ast.Expr)
                and isinstance(stmt_n.value, ast.Call)
                and isinstance(stmt_n.value.func, ast.Name)
                and stmt_n.value.func.id == "load"
                and stmt_n.value.args
                and isinstance(stmt_n.value.args[0], ast.Constant)
                and isinstance(stmt_n.value.args[0].value, str)
            ):
                continue
            bzl_lbl = stmt_n.value.args[0].value
            if not bzl_lbl.startswith("//") or not bzl_lbl.endswith(".bzl"):
                continue
            bzl_rel = bzl_lbl[2:].replace(":", "/")
            bzl_path = os.path.join(workdir, bzl_rel)
            if not os.path.isfile(bzl_path):
                continue
            try:
                bzl_txt = open(bzl_path, encoding="utf-8", errors="ignore").read()
            except OSError:
                continue
            # Follow one forwarding load hop (e.g. public re-export wrapper .bzl).
            if "macro(" not in bzl_txt and "rule(" not in bzl_txt:
                m_fwd = re.search(r'load\(\s*"(//[^"]+\.bzl)"', bzl_txt)
                if m_fwd:
                    fwd_path = os.path.join(workdir, m_fwd.group(1)[2:].replace(":", "/"))
                    if os.path.isfile(fwd_path):
                        try:
                            bzl_txt = open(fwd_path, encoding="utf-8", errors="ignore").read()
                        except OSError:
                            pass
            if "HOST_CONSTRAINTS" not in bzl_txt and "HOST_OS_CONSTRAINTS" not in bzl_txt:
                continue
            syms = [
                a.value
                for a in stmt_n.value.args[1:]
                if isinstance(a, ast.Constant) and isinstance(a.value, str)
            ] + [kw.arg for kw in stmt_n.value.keywords if kw.arg]
            for sym in syms:
                m_mac = re.search(
                    r"\b" + re.escape(sym) + r"\s*=\s*macro\s*\((.*?)\n\)",
                    bzl_txt,
                    re.DOTALL,
                )
                if not m_mac:
                    continue
                m_body = m_mac.group(1)
                drops_tcw = bool(re.search(r'"target_compatible_with"\s*:\s*None\b', m_body))
                omits_tcw = (
                    "inherit_attrs" not in m_body and '"target_compatible_with"' not in m_body
                )
                sets_host = bool(
                    re.search(
                        r"target_compatible_with\s*=\s*HOST_(?:OS_)?CONSTRAINTS\b", bzl_txt
                    )
                )
                if (drops_tcw or omits_tcw) and sets_host:
                    forbid_tcw.add(sym)
        return forbid_tcw

    # Check whether BUILD.gn has a file-scope (brace-depth 0) host-only assert(...).
    def has_gn_file_scope_host_assert(gn_src):
        """Return True when `gn_src` has a top-level host-only `assert(...)`."""
        cleaned = "\n".join(
            re.sub(r'("(?:\\.|[^"\\])*")|#.*$', lambda m: m.group(1) or "", ln)
            for ln in gn_src.splitlines()
        )
        depth = 0
        host_assert_pat = (
            r'"(?:\\.|[^"\\])*"|\{|\}|\bassert\s*\(\s*'
            r"(?:is_host|!\s*is_fuchsia|current_toolchain\s*==\s*host_toolchain)\b"
        )
        for tok_m in re.finditer(host_assert_pat, cleaned):
            tok = tok_m.group(0)
            if tok.startswith('"'):
                continue
            if tok == "{":
                depth += 1
            elif tok == "}":
                depth = max(0, depth - 1)
            elif depth == 0:
                return True
        return False

    # Extract GN target names declared inside `if (is_host)` blocks or files with `assert(is_host)`.
    def find_gn_is_host_targets(gn_src):
        """Return target names inside `if (is_host)` blocks or `assert(is_host)` files."""
        cleaned = "\n".join(
            ln for ln in gn_src.splitlines() if not ln.lstrip().startswith("#")
        )
        names = set()
        host_cond_pat = r"(?:is_host|!\s*is_fuchsia|current_toolchain\s*==\s*host_toolchain)"
        if re.search(
            r"(?m)^\s*assert\s*\(\s*" + host_cond_pat + r"\b(?:\s*\)|\s*,)",
            cleaned,
        ):
            for m_t in re.finditer(r'\b[A-Za-z_]\w*\(\s*"([^"]+)"\s*\)\s*\{', cleaned):
                names.add(m_t.group(1))
                names.add(f"{m_t.group(1)}_test")
        for m_host in re.finditer(r"\bif\s*\(\s*" + host_cond_pat + r"\s*\)\s*\{", cleaned):
            depth, idx = 1, m_host.end()
            while idx < len(cleaned) and depth > 0:
                if cleaned[idx] == "{":
                    depth += 1
                elif cleaned[idx] == "}":
                    depth -= 1
                idx += 1
            blk = cleaned[m_host.end() : max(m_host.end(), idx - 1)]
            for m_t in re.finditer(r'\b[A-Za-z_]\w*\(\s*"([^"]+)"\s*\)\s*\{', blk):
                names.add(m_t.group(1))
                if "with_unit_tests" in blk:
                    names.add(f"{m_t.group(1)}_test")
        return names

    # Check host platform constraint conventions (HOST_OS_CONSTRAINTS vs HOST_CONSTRAINTS,
    # rejecting unsupported target_compatible_with on host-only symbolic macros, and
    # preserving `if (is_host)` guards on migrated targets).
    def check_host_constraints_parity(tree_node, gn_text):
        """Verify host-only targets use HOST_OS_CONSTRAINTS and preserve GN is_host guards."""
        cur_gn_path = os.path.join(workdir, pkg_dir, "BUILD.gn")
        is_dual_build_pkg = False
        if os.path.isfile(cur_gn_path):
            try:
                is_dual_build_pkg = "## BAZEL2GN SENTINEL" in open(
                    cur_gn_path, encoding="utf-8", errors="ignore"
                ).read()
            except OSError:
                pass
        # Legacy wrapper macros that forward **kwargs (such as host_go_test -> go_test and
        # legacy_host_py_test -> py_binary) accept/require target_compatible_with for host
        # constraint parity; symbolic macros that drop/omit target_compatible_with are caught
        # dynamically below.
        forbid_tcw_macros = {
            "ffx_plugin",
            "idk_ffx_tool",
            "host_rustc_test",
            "wrap_host_rust_test",
            "host_cc_test",
            "host_shac_test",
        } | resolve_workspace_forbid_tcw_macros(tree_node)

        for stmt_node in tree_node.body:
            if not (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
            ):
                continue
            call_n = stmt_node.value
            seg = ast.get_source_segment(source, stmt_node) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            if (
                call_n.func.id == "load"
                and call_n.args
                and isinstance(call_n.args[0], ast.Constant)
                and call_n.args[0].value == "@platforms//host:constraints.bzl"
            ):
                _emit(findings, "disallowed_host_constraints", "error", rel_path, getattr(stmt_node, "lineno", 1), f"'{rel_path}' loads `@platforms//host:constraints.bzl` (`HOST_CONSTRAINTS`), "
                        "which must not be used when migrating host targets.", f"Replace `load(\"@platforms//host:constraints.bzl\", \"HOST_CONSTRAINTS\")` "
                        "and `target_compatible_with = HOST_CONSTRAINTS` in "
                        f"'{rel_path}' with `load(\"//build/bazel/platforms:constraints.bzl\", "
                        "\"HOST_OS_CONSTRAINTS\")` and `target_compatible_with = HOST_OS_CONSTRAINTS`.")
            elif call_n.func.id in forbid_tcw_macros:
                t_name = ""
                tcw_line = 0
                for kw in call_n.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        t_name = kw.value.value
                    elif kw.arg == "target_compatible_with":
                        tcw_line = getattr(kw, "lineno", getattr(call_n, "lineno", 1))
                if tcw_line:
                    _emit(findings, "unsupported_target_compatible_with", "error", rel_path, tcw_line, f"Target '{t_name or call_n.func.id}' ({call_n.func.id}) in "
                            f"'{rel_path}' sets `target_compatible_with`, which host-only "
                            f"macro `{call_n.func.id}` does not accept (it sets host "
                            "constraints internally).", f"Remove `target_compatible_with` from `{call_n.func.id}` in "
                            f"'{rel_path}' and drop any unused `HOST_OS_CONSTRAINTS` load.")

        prev_gn = gn_text
        if "## BAZEL2GN SENTINEL" in prev_gn or not prev_gn:
            for fallback_ref in ("HEAD~1", "origin/main"):
                cand_gn = "\n".join(git_lines(["show", f"{fallback_ref}:{pkg_dir}/BUILD.gn"]))
                if cand_gn and "## BAZEL2GN SENTINEL" not in cand_gn:
                    prev_gn = cand_gn
                    break
                if cand_gn and not prev_gn:
                    prev_gn = cand_gn

        # Flag `# @bazel2gn:skip` on `target_compatible_with = HOST_OS_CONSTRAINTS` (or comments
        # above the target or attribute falsely claiming a GN `is_host`-guarded target was not
        # gated by `is_host` in GN) on convertible targets that do not have a
        # `@platforms//os:fuchsia` select() branch: `bazel2gn` natively maps
        # `target_compatible_with = HOST_OS_CONSTRAINTS` to `if (is_host)`.
        file_is_host_only = bool(prev_gn and has_gn_file_scope_host_assert(prev_gn))
        host_guarded_names = (
            find_gn_is_host_targets(prev_gn)
            if prev_gn
            and re.search(
                r"\b(?:if|assert)\s*\(\s*"
                r"(?:is_host|!\s*is_fuchsia|current_toolchain\s*==\s*host_toolchain)\b",
                prev_gn,
            )
            else set()
        )
        convertible_host_tcw_rules = {
            "alias",
            "rustc_library",
            "rustc_binary",
            "rustc_dylib",
            "rustc_proc_macro",
            "rustc_staticlib",
            "rustc_test",
            "host_rustc_test",
            "fx_cc_library",
            "fx_cc_binary",
            "fx_cc_test_binary",
            "cc_library",
            "cc_binary",
            "go_library",
            "go_binary",
            "py_library",
            "py_binary",
        }
        false_not_wrapped_re = re.compile(
            r"(?:did\s+not|does\s+not|was\s+not|not)\s+"
            r"(?:wrap\w*|gat\w*|guard\w*)[^\n]*\bis_host\b"
            r"|\bTODO\b[^\n]*(?:target_compatible_with|HOST_OS_CONSTRAINTS|\bis_host\b)",
            re.I,
        )
        for stmt_node in tree_node.body:
            if not (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
                and stmt_node.value.func.id in convertible_host_tcw_rules
            ):
                continue
            call_n = stmt_node.value
            r_id = call_n.func.id
            call_seg = ast.get_source_segment(source, stmt_node) or ""
            if "@platforms//os:fuchsia" in call_seg:
                continue
            t_name = ""
            tcw_kw = None
            for kw in call_n.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    t_name = kw.value.value
                elif kw.arg == "target_compatible_with":
                    tcw_kw = kw
            if tcw_kw is None:
                continue
            tcw_seg = ast.get_source_segment(source, tcw_kw) or ""
            if "HOST_OS_CONSTRAINTS" not in tcw_seg or is_preexisting_target_attr(r_id, t_name, tcw_seg):
                continue
            # Check if the target itself is marked # @bazel2gn:skip above the rule call,
            # and collect contiguous comments directly above the target call.
            tgt_skipped = False
            comment_lines_above_tgt = []
            saw_blank_above_tgt = False
            idx_above = getattr(stmt_node, "lineno", 1) - 2
            while idx_above >= 0:
                s_ln = src_lines[idx_above].strip()
                if not s_ln:
                    saw_blank_above_tgt = True
                    idx_above -= 1
                    continue
                if s_ln.startswith("#"):
                    if "@bazel2gn:skip" in s_ln:
                        tgt_skipped = True
                    if not saw_blank_above_tgt and "@bazel2gn:" not in s_ln:
                        comment_lines_above_tgt.append((idx_above + 1, s_ln))
                    idx_above -= 1
                    continue
                break
            if tgt_skipped:
                continue
            kw_start = getattr(tcw_kw, "lineno", 1)
            kw_end = getattr(tcw_kw, "end_lineno", kw_start)
            kw_lines = src_lines[max(0, kw_start - 1) : min(len(src_lines), kw_end)]
            has_attr_skip = any("@bazel2gn:skip" in ln for ln in kw_lines)
            comment_lines_above_kw = []
            c_idx = kw_start - 2
            while c_idx >= 0 and src_lines[c_idx].strip().startswith("#"):
                comment_lines_above_kw.append((c_idx + 1, src_lines[c_idx].strip()))
                c_idx -= 1
            if c_idx >= 0 and "@bazel2gn:skip" in src_lines[c_idx]:
                has_attr_skip = True
            ordered_tgt_comments = list(reversed(comment_lines_above_tgt))
            ordered_kw_comments = list(reversed(comment_lines_above_kw))
            unwrapped_tgt_comments = " ".join(
                txt.lstrip("#").strip() for _, txt in ordered_tgt_comments
            )
            unwrapped_kw_comments = " ".join(
                txt.lstrip("#").strip() for _, txt in ordered_kw_comments
            )
            is_host_in_gn = file_is_host_only or (t_name in host_guarded_names)
            has_false_tgt_comment = is_host_in_gn and bool(
                false_not_wrapped_re.search(unwrapped_tgt_comments)
            )
            has_false_kw_comment = is_host_in_gn and bool(
                false_not_wrapped_re.search(unwrapped_kw_comments)
            )
            has_false_comment = has_false_tgt_comment or has_false_kw_comment
            if has_attr_skip:
                _emit(
                    findings,
                    "skipped_host_os_constraints",
                    "error",
                    rel_path,
                    kw_start,
                    (
                        f"Target '{t_name}' ({r_id}) in '{rel_path}' puts `# @bazel2gn:skip` on "
                        "`target_compatible_with = HOST_OS_CONSTRAINTS`. `bazel2gn` natively "
                        "translates `target_compatible_with = HOST_OS_CONSTRAINTS` into "
                        "`if (is_host)` in BUILD.gn; skipping it leaves the generated GN target "
                        "unguarded."
                    ),
                    (
                        "Remove `# @bazel2gn:skip` from `target_compatible_with = "
                        "HOST_OS_CONSTRAINTS` and delete any skip/TODO comment on "
                        f"'{t_name}' in '{rel_path}' (if `{t_name}` was split into `rustc_test` "
                        "+ `wrap_host_rust_test` solely to skip `HOST_OS_CONSTRAINTS`, collapse "
                        f"it back to `host_rustc_test(name = \"{t_name}\", ...)` and update any "
                        "ancestor `test_suite` entry), then re-run "
                        f"`fx bazel2gn -d {pkg_dir}` so `bazel2gn` wraps `{t_name}` in "
                        "`if (is_host)` in BUILD.gn."
                    ),
                )
            elif has_false_comment:
                if has_false_tgt_comment and ordered_tgt_comments:
                    rep_line = next(
                        (
                            ln
                            for ln, txt in ordered_tgt_comments
                            if re.search(
                                r"(?:TODO|Note|target_compatible_with|is_host)", txt, re.I
                            )
                        ),
                        ordered_tgt_comments[0][0],
                    )
                elif has_false_kw_comment and ordered_kw_comments:
                    rep_line = ordered_kw_comments[0][0]
                else:
                    rep_line = kw_start
                _emit(
                    findings,
                    "skipped_host_os_constraints",
                    "error",
                    rel_path,
                    rep_line,
                    (
                        f"Target '{t_name}' ({r_id}) in '{rel_path}' has a comment/TODO "
                        f"claiming `{t_name}` was not gated by `is_host` in GN, whereas "
                        f"`{pkg_dir}/BUILD.gn` already restricted it to host (`assert(is_host)` "
                        "or `if (is_host)`)."
                    ),
                    (
                        f"Remove the stale `target_compatible_with` TODO/comment on '{t_name}' "
                        f"in '{rel_path}' while keeping `target_compatible_with = "
                        "HOST_OS_CONSTRAINTS` synced without `# @bazel2gn:skip`."
                    ),
                )

        if not host_guarded_names:
            return
        host_lib_bin_rules = {
            "rustc_library",
            "rustc_binary",
            "fx_cc_library",
            "fx_cc_binary",
            "fx_cc_test_binary",
            "cc_library",
            "cc_binary",
            "go_library",
            "go_binary",
            "go_binary_host_tool",
            "py_library",
            "py_binary",
        }
        if "host_py_test" not in forbid_tcw_macros:
            host_lib_bin_rules.add("host_py_test")
        if is_dual_build_pkg:
            host_lib_bin_rules.add("idk_go_binary_host_tool")
        for stmt_node in tree_node.body:
            if not (
                isinstance(stmt_node, ast.Expr)
                and isinstance(stmt_node.value, ast.Call)
                and isinstance(stmt_node.value.func, ast.Name)
            ):
                continue
            call_n = stmt_node.value
            rule_id = call_n.func.id
            t_name = ""
            has_tcw = False
            for kw in call_n.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    t_name = kw.value.value
                elif kw.arg == "target_compatible_with":
                    has_tcw = True
            if not t_name or has_tcw:
                continue
            seg = ast.get_source_segment(source, stmt_node) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            if rule_id in host_lib_bin_rules and t_name in host_guarded_names:
                _emit(findings, "missing_host_os_constraints", "error", rel_path, getattr(call_n, "lineno", 1), f"Target '{t_name}' ({rule_id}) in '{rel_path}' was guarded by "
                        f"`if (is_host)` in `{pkg_dir}/BUILD.gn` but omits "
                        "`target_compatible_with = HOST_OS_CONSTRAINTS`.", "Add `load(\"//build/bazel/platforms:constraints.bzl\", "
                        "\"HOST_OS_CONSTRAINTS\")` and `target_compatible_with = "
                        f"HOST_OS_CONSTRAINTS` on '{t_name}' in '{rel_path}', then "
                        f"re-run `fx bazel2gn -d {pkg_dir}` if dual-building.")

    # Check that host tools bridged into GN via `bazel_ffx_tool` or `bazel_host_tool` AND
    # cross-compiled for `host_arm64_toolchain` (`//build/bazel/platforms:host_linux_arm64`)
    # via `//bundles/infra/test:host_tools` (`//src/developer/ffx/tools:bins`) or the IDK
    # (`//sdk:host_tools_for_configured_host_cpus` / `sdk_atom` / `sdk_host_tool`) do not
    # depend transitively on Bazel targets constrained to `HOST_CONSTRAINTS` (`cpu:x86_64`).
    def is_cross_compiled_arm64_host_tool_pkg(p_dir: str, gn_clean_txt: str) -> bool:
        if re.search(r"\b(?:sdk_atom|sdk_host_tool)\s*\(|\bhost_arm64_toolchain\b", gn_clean_txt):
            return True
        leaf = os.path.basename(p_dir)
        for reg_rel, allow_rel_leaf in (
            ("bundles/infra/test/BUILD.gn", False),
            ("src/developer/ffx/tools/BUILD.gn", p_dir.startswith("src/developer/ffx/tools/")),
            ("src/developer/ffx/config.gni", False),
            ("sdk/BUILD.gn", False),
        ):
            reg_path = os.path.join(workdir, reg_rel)
            try:
                reg_txt = re.sub(
                    r"#[^\n]*", "", open(reg_path, encoding="utf-8", errors="ignore").read()
                )
            except OSError:
                continue
            if re.search(r'"//' + re.escape(p_dir) + r'(?=[":])', reg_txt):
                return True
            if allow_rel_leaf and re.search(r'"' + re.escape(leaf) + r'(?=[":])', reg_txt):
                return True
        return False

    def check_host_tool_transitive_constraints(tree_node):
        """Verify cross-compiled bridged host tools do not depend on HOST_CONSTRAINTS targets."""
        cur_gn_path = os.path.join(workdir, pkg_dir, "BUILD.gn")
        if not os.path.isfile(cur_gn_path):
            return
        try:
            cur_gn_src = open(cur_gn_path, encoding="utf-8", errors="ignore").read()
        except OSError:
            return
        gn_clean = re.sub(r"#[^\n]*", "", cur_gn_src)
        if not is_cross_compiled_arm64_host_tool_pkg(pkg_dir, gn_clean):
            return
        bridged_targets = []
        for m_br in re.finditer(
            r'\b(bazel_ffx_tool|bazel_host_tool)\(\s*"([^"]+)"\s*\)\s*\{([^}]*)\}',
            gn_clean,
        ):
            tmpl_name, gn_tname, body_txt = m_br.group(1), m_br.group(2), m_br.group(3)
            m_bt = re.search(r'\bbazel_target\s*=\s*"([^"]+)"', body_txt)
            if m_bt:
                raw_bt = m_bt.group(1).strip()
                bt_name = raw_bt.split(":")[-1]
                if bt_name.endswith("_versioned"):
                    bt_name = bt_name[: -len("_versioned")]
            else:
                bt_name = gn_tname
            bridged_targets.append((tmpl_name, gn_tname, bt_name))
        if not bridged_targets:
            return

        pkg_cache = {}
        bzl_macro_cache = {}
        # Exclude `proc_macro_deps` and procedural macro rules: Bazel transitions
        # procedural macros to the `exec` configuration rather than the cross-compiled
        # `host_linux_arm64` target configuration, so `HOST_CONSTRAINTS` on proc-macro
        # crates and their exec-only helpers does not block target cross-compilation.
        exec_only_rule_names = {"rustc_proc_macro", "rust_proc_macro"}
        dep_kw_names = {
            "deps",
            "implementation_deps",
            "public_deps",
            "non_test_deps",
            "link_deps",
            "args_deps",
            "plugin_deps",
            "crate",
        }

        # Inspect a workspace .bzl file for @platforms//host:constraints.bzl usage
        # and target-configured workspace dependency labels injected by its macros.
        def inspect_workspace_bzl(bzl_rel):
            if bzl_rel in bzl_macro_cache:
                return bzl_macro_cache[bzl_rel]
            bzl_abs = os.path.join(workdir, bzl_rel)
            if not os.path.isfile(bzl_abs):
                bzl_macro_cache[bzl_rel] = (False, [])
                return False, []
            try:
                bzl_src = open(bzl_abs, encoding="utf-8", errors="ignore").read()
                bzl_ast = ast.parse(bzl_src, filename=bzl_abs)
            except Exception:
                bzl_macro_cache[bzl_rel] = (False, [])
                return False, []
            uses_host_cpu = False
            top_consts = {}
            for st in bzl_ast.body:
                if (
                    isinstance(st, ast.Expr)
                    and isinstance(st.value, ast.Call)
                    and isinstance(st.value.func, ast.Name)
                    and st.value.func.id == "load"
                    and st.value.args
                    and isinstance(st.value.args[0], ast.Constant)
                    and st.value.args[0].value == "@platforms//host:constraints.bzl"
                ):
                    uses_host_cpu = bool(re.search(r"\bHOST_CONSTRAINTS\b", bzl_src))
                elif isinstance(st, ast.Assign):
                    vals = [
                        c.value
                        for c in ast.walk(st.value)
                        if isinstance(c, ast.Constant) and isinstance(c.value, str)
                    ]
                    for tgt in st.targets:
                        if isinstance(tgt, ast.Name):
                            top_consts[tgt.id] = vals
            injected_deps = []
            for st in bzl_ast.body:
                if not isinstance(st, ast.FunctionDef):
                    continue
                for sub in ast.walk(st):
                    if not isinstance(sub, ast.Call):
                        continue
                    for kw in sub.keywords:
                        if kw.arg not in dep_kw_names:
                            continue
                        for item in ast.walk(kw.value):
                            cands = []
                            if isinstance(item, ast.Constant) and isinstance(item.value, str):
                                cands.append(item.value)
                            elif isinstance(item, ast.Name) and item.id in top_consts:
                                cands.extend(top_consts[item.id])
                            for s in cands:
                                if (
                                    s.startswith("//")
                                    and not s.startswith(("//third_party/", "//build/"))
                                    and " " not in s
                                    and s not in injected_deps
                                ):
                                    injected_deps.append(s)
            bzl_macro_cache[bzl_rel] = (uses_host_cpu, injected_deps)
            return uses_host_cpu, injected_deps

        # Parse top-level target calls, macro loads, and dependency edges in <p_dir>/BUILD.bazel.
        def parse_pkg_targets(p_dir):
            if p_dir in pkg_cache:
                return pkg_cache[p_dir]
            bz_file = os.path.join(workdir, p_dir, "BUILD.bazel")
            if not os.path.isfile(bz_file):
                pkg_cache[p_dir] = {}
                return {}
            try:
                bz_src = open(bz_file, encoding="utf-8", errors="ignore").read()
                bz_ast = ast.parse(bz_src, filename=bz_file)
            except Exception:
                pkg_cache[p_dir] = {}
                return {}
            sym_to_bzl = {}
            pkg_vars = {}
            for st in bz_ast.body:
                if (
                    isinstance(st, ast.Expr)
                    and isinstance(st.value, ast.Call)
                    and isinstance(st.value.func, ast.Name)
                    and st.value.func.id == "load"
                    and st.value.args
                    and isinstance(st.value.args[0], ast.Constant)
                    and isinstance(st.value.args[0].value, str)
                ):
                    mod_s = st.value.args[0].value
                    if mod_s.startswith("//") and mod_s.endswith(".bzl"):
                        b_pkg, _, b_file = mod_s[2:].partition(":")
                        bzl_rel = f"{b_pkg.strip('/')}/{b_file.lstrip('/')}" if b_file else ""
                        if bzl_rel:
                            for a in st.value.args[1:]:
                                if isinstance(a, ast.Constant) and isinstance(a.value, str):
                                    sym_to_bzl[a.value] = bzl_rel
                            for kw in st.value.keywords:
                                if kw.arg:
                                    sym_to_bzl[kw.arg] = bzl_rel
                elif isinstance(st, ast.Assign):
                    vals = [
                        c.value
                        for c in ast.walk(st.value)
                        if isinstance(c, ast.Constant) and isinstance(c.value, str)
                    ]
                    for tgt in st.targets:
                        if isinstance(tgt, ast.Name):
                            pkg_vars[tgt.id] = vals
            res = {}
            for st in bz_ast.body:
                if not (
                    isinstance(st, ast.Expr)
                    and isinstance(st.value, ast.Call)
                    and isinstance(st.value.func, ast.Name)
                ):
                    continue
                c_node = st.value
                r_id = c_node.func.id
                is_exec_rule = r_id in exec_only_rule_names
                t_nm = ""
                t_deps = []
                has_host_cpu_tcw = False
                macro_bzl = ""
                if not is_exec_rule and r_id in sym_to_bzl:
                    bzl_bad, bzl_deps = inspect_workspace_bzl(sym_to_bzl[r_id])
                    if bzl_bad:
                        macro_bzl = sym_to_bzl[r_id]
                    t_deps.extend(bzl_deps)
                for kw in c_node.keywords:
                    if (
                        kw.arg == "name"
                        and isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                    ):
                        t_nm = kw.value.value
                    elif not is_exec_rule and kw.arg in dep_kw_names:
                        for sub in ast.walk(kw.value):
                            if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                                t_deps.append(sub.value)
                            elif isinstance(sub, ast.Name) and sub.id in pkg_vars:
                                t_deps.extend(pkg_vars[sub.id])
                    elif not is_exec_rule and kw.arg == "target_compatible_with":
                        tcw_txt = ast.get_source_segment(bz_src, kw.value) or ""
                        if re.search(r"\bHOST_CONSTRAINTS\b", tcw_txt):
                            has_host_cpu_tcw = True
                if t_nm:
                    res[t_nm] = (getattr(st, "lineno", 1), has_host_cpu_tcw, macro_bzl, t_deps)
            pkg_cache[p_dir] = res
            return res

        local_tgts = parse_pkg_targets(pkg_dir)
        for tmpl_name, gn_tname, bt_name in bridged_targets:
            if bt_name not in local_tgts:
                continue
            bt_line = local_tgts[bt_name][0]
            visited = set()
            queue = [(pkg_dir, bt_name)]
            bad_bzls = []
            bad_labels = []
            while queue:
                cur_p, cur_t = queue.pop(0)
                p_tgts = parse_pkg_targets(cur_p)
                resolved_t = cur_t if cur_t in p_tgts else ""
                if not resolved_t:
                    for suf in ("_args", "_sub_command", "_versioned"):
                        if cur_t.endswith(suf) and cur_t[: -len(suf)] in p_tgts:
                            resolved_t = cur_t[: -len(suf)]
                            break
                if not resolved_t or (cur_p, resolved_t) in visited:
                    continue
                visited.add((cur_p, resolved_t))
                _, is_bad_tcw, m_bzl, dep_list = p_tgts[resolved_t]
                if m_bzl and m_bzl not in bad_bzls:
                    bad_bzls.append(m_bzl)
                if is_bad_tcw and (cur_p, resolved_t) != (pkg_dir, bt_name):
                    lbl_s = f"//{cur_p}:{resolved_t}"
                    if lbl_s not in bad_labels:
                        bad_labels.append(lbl_s)
                for d_lbl in dep_list:
                    if d_lbl.startswith(":"):
                        queue.append((cur_p, d_lbl[1:]))
                    elif d_lbl.startswith("//") and not d_lbl.startswith("//third_party/"):
                        dep_p, _, dep_t = d_lbl[2:].partition(":")
                        dep_p = dep_p.strip("/")
                        dep_t = dep_t or os.path.basename(dep_p)
                        queue.append((dep_p, dep_t))
            if bad_bzls or bad_labels:
                all_items = [f"'{b}'" for b in bad_bzls] + [f"`{lbl}`" for lbl in bad_labels]
                sample = ", ".join(all_items[:6])
                more = f" (and {len(all_items) - 6} more)" if len(all_items) > 6 else ""
                full_list = ", ".join(bad_bzls + bad_labels)
                msg = (
                    f"Target '{bt_name}' in '{rel_path}' is bridged into `{pkg_dir}/BUILD.gn` "
                    f"via `{tmpl_name}(\"{gn_tname}\")`, which cross-compiles for "
                    "`//build/bazel/platforms:host_linux_arm64`, but its transitive Bazel "
                    f"dependency/macro closure uses `HOST_CONSTRAINTS` "
                    f"(`@@platforms//cpu:x86_64`): {sample}{more}."
                )
                rem = (
                    f"In all blocking collateral `.bzl` and `BUILD.bazel` files ({full_list}), "
                    'replace `load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")` '
                    "and `target_compatible_with = HOST_CONSTRAINTS` with "
                    '`load("//build/bazel/platforms:constraints.bzl", '
                    '"HOST_OS_CONSTRAINTS")` and `target_compatible_with = '
                    "HOST_OS_CONSTRAINTS` (updating any pre-existing `*:defs.bzl` load to its "
                    "direct rule `.bzl` file if required by `bazel_shac_lint`, making no other "
                    "edits to those files or their `BUILD.gn` files, and omitting collateral "
                    "packages from `Bazel-Migration-Target:` and `Also migrated in this "
                    "change:`)."
                )
                _emit(
                    findings,
                    "host_tool_transitive_host_constraints",
                    "error",
                    rel_path,
                    bt_line,
                    msg,
                    rem,
                )

    # Check that device-only rustc_test sets target_compatible_with = ["@platforms//os:fuchsia"]
    # (tags = ["manual"] is no substitute; migration_sanity unnecessary_manual_tag allows it on
    # a rustc_test only when a Fuchsia test package wraps it or the package is Bazel-only) so
    # host wildcard builds/tests skip incompatible targets cleanly and bazel2gn emits
    # `if (is_fuchsia)`.
    def check_fuchsia_only_rust_bin_and_test_attrs(tree_node, gn_text):
        """Verify Fuchsia-only rustc_test targets set required constraints."""
        fuchsia_only_deps = {
            "//sdk/rust/zx",
            "//src/lib/fdio/rust",
            "//src/lib/fuchsia",
            "//src/lib/fuchsia-component",
            "//src/lib/fuchsia-runtime",
            "//src/zircon/lib/zircon",
        }
        packaged_bins = set()
        host_wrapped_tests = set()
        crate_deps = {}
        for stmt_n in tree_node.body:
            if not (
                isinstance(stmt_n, ast.Expr)
                and isinstance(stmt_n.value, ast.Call)
                and isinstance(stmt_n.value.func, ast.Name)
            ):
                continue
            c_node = stmt_n.value
            r_id = c_node.func.id
            c_name = ""
            c_deps = set()
            for kw in c_node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    c_name = kw.value.value
                elif kw.arg == "deps":
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            c_deps.add(sub.value)
                elif (
                    r_id in ("fx_packaged_binary", "fx_unittest_component")
                    and kw.arg == "binary"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    packaged_bins.add(kw.value.value.lstrip(":"))
                elif r_id == "fx_unittest_package" and kw.arg == "unit_tests":
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            packaged_bins.add(sub.value.lstrip(":"))
                elif (
                    r_id in ("wrap_host_rust_test", "host_test")
                    and kw.arg in ("binary_name", "binary")
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    host_wrapped_tests.add(kw.value.value.lstrip(":"))
            if c_name and r_id.startswith(("rustc_", "rust_")):
                crate_deps[c_name] = c_deps

        for stmt_n in tree_node.body:
            if not (
                isinstance(stmt_n, ast.Expr)
                and isinstance(stmt_n.value, ast.Call)
                and isinstance(stmt_n.value.func, ast.Name)
            ):
                continue
            c_node = stmt_n.value
            r_id = c_node.func.id
            if r_id not in ("rustc_binary", "rust_binary", "rustc_test", "rust_test"):
                continue
            seg = ast.get_source_segment(source, stmt_n) or ""
            if seg and re.sub(r"\s+", "", seg) in base_bazel_flat:
                continue
            t_name = ""
            crate_ref = ""
            tags_vals = set()
            tcw_vals = set()
            has_tcw = False
            own_deps = set()
            for kw in c_node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    t_name = kw.value.value
                elif (
                    kw.arg == "crate"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    crate_ref = kw.value.value.lstrip(":")
                elif kw.arg == "tags":
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            tags_vals.add(sub.value)
                elif kw.arg == "target_compatible_with":
                    has_tcw = True
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            tcw_vals.add(sub.value)
                        elif isinstance(sub, ast.Name):
                            tcw_vals.add(sub.id)
                elif kw.arg == "deps":
                    for sub in ast.walk(kw.value):
                        if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                            own_deps.add(sub.value)
            if not t_name or is_preexisting_target_attr(r_id, t_name):
                continue
            eff_deps = own_deps | crate_deps.get(crate_ref, set())
            target_only_used = sorted(eff_deps & fuchsia_only_deps)
            if r_id in ("rustc_test", "rust_test"):
                if (
                    t_name in host_wrapped_tests
                    or bool(tcw_vals & {"HOST_OS_CONSTRAINTS", "HOST_CONSTRAINTS"})
                    or re.search(
                        r'":' + re.escape(t_name) + r'\(\$host_toolchain\)"', gn_text
                    )
                ):
                    continue
                if (
                    t_name in packaged_bins
                    and target_only_used
                    and "@platforms//os:fuchsia" not in tcw_vals
                ):
                    dep_str = ", ".join(f"`{d}`" for d in target_only_used)
                    msg = (
                        f"Target '{t_name}' ({r_id}) in '{rel_path}' is a Fuchsia "
                        f"device test binary depending on target-only crates "
                        f"({dep_str}) without "
                        '`target_compatible_with = ["@platforms//os:fuchsia"]`. '
                        "Without this constraint, Bazel includes the raw test binary "
                        "in host wildcard test runs where target-only dependencies "
                        "fail to build, and `bazel2gn` omits the `if (is_fuchsia)` "
                        "guard in `BUILD.gn`."
                    )
                    rem = (
                        "Add `target_compatible_with = [\"@platforms//os:fuchsia\"]` "
                        f"to `{r_id}(name = \"{t_name}\")` "
                        f"in '{rel_path}' and re-run `fx bazel2gn -d {pkg_dir}` if "
                        "dual-building. `tags = [\"manual\"]` does not replace this "
                        "constraint (keep a `manual` tag only if a Fuchsia test package "
                        "wraps this `rustc_test` or the package has no "
                        "`## BAZEL2GN SENTINEL`)."
                    )
                    _emit(
                        findings,
                        "missing_fuchsia_rust_test_constraints",
                        "error",
                        rel_path,
                        getattr(c_node, "lineno", 1),
                        msg,
                        rem,
                    )

    # Check that fx_component and fx_test_component targets do not set manifest-only
    # attributes (component_name, manifest, includes, restricted_features) or
    # disabled/misplaced symbolic-macro attributes (is_test, test_realm, moniker,
    # is_driver, or test_type on non-test fx_component).
    def _emit_disabled_fx_component_attr(r_id, t_name, attr_name, lineno):
        """Emit an error for symbolic-macro attributes set to None or wrong macro."""
        if attr_name == "test_type":
            detail = (
                "`test_type` is only accepted on `fx_test_component`, never on "
                "`fx_component` (`Error: no such attribute 'test_type' in "
                "'fx_component' macro`)."
            )
            rem = (
                f"If `{t_name}` is listed in the `test_components` of an `fx_package` in "
                f"'{rel_path}', change `fx_component(name = \"{t_name}\")` to "
                f"`fx_test_component` and re-run `fx bazel2gn -d {pkg_dir}` if "
                "dual-building. Otherwise (its only users are GN test packages) delete the "
                "Bazel target and its Bazel-only manifest/binary helpers and keep the GN "
                "`fuchsia_test_component` hand-written above `## BAZEL2GN SENTINEL` (no "
                "`# @bazel2gn:skip` twin): dropping `test_type` changes the GN template "
                "(gn_target_type_parity) and an unpackaged `fx_test_component` is not a "
                "test root (migration_sanity test_component_not_test_root)."
            )
        else:
            detail = (
                f"In `//build/bazel/rules/components:fx_component.bzl`, "
                f"`{attr_name}` is explicitly disabled (`None`) on `{r_id}` and "
                f"fails package loading (`Error: no such attribute '{attr_name}' "
                f"in '{r_id}' macro`)."
            )
            rem = (
                f"Remove `{attr_name}` from `{r_id}(name = \"{t_name}\")` in "
                f"'{rel_path}' (for test components, use `fx_test_component` with "
                f"`test_type` rather than `is_test`/`test_realm`) and re-run "
                f"`fx bazel2gn -d {pkg_dir}` if dual-building."
            )
        _emit(
            findings,
            "unsupported_fx_component_attribute",
            "error",
            rel_path,
            lineno,
            f"Target '{t_name}' ({r_id}) in '{rel_path}' sets unsupported attribute "
            f"`{attr_name}`. {detail}",
            rem,
        )

    # Load live bazel2gn attribute allowlists from build/tools/bazel2gn/fx_component.go
    # so upstream attribute removals or additions are enforced without manual list drift.
    def _load_bazel2gn_component_allowlists() -> dict[str, set[str]]:
        go_path = os.path.join(workdir, "build", "tools", "bazel2gn", "fx_component.go")
        try:
            go_src = open(go_path, encoding="utf-8").read()
        except OSError:
            return {}
        var_to_rule = {
            "allowedFxComponentAttrs": "fx_component",
            "allowedFxComponentManifestAttrs": "fx_component_manifest",
            "allowedFxCompiledManifestAttrs": "fx_compiled_manifest",
            "allowedFxPackagedBinaryAttrs": "fx_packaged_binary",
        }
        allowlists: dict[str, set[str]] = {}
        map_re = re.compile(
            r"\b(allowedFx[A-Za-z]+Attrs)\s*=\s*map\[string\]bool\s*\{([^}]*)\}"
        )
        for m in map_re.finditer(go_src):
            var_name, body = m.group(1), m.group(2)
            rule_name = var_to_rule.get(var_name)
            if not rule_name:
                continue
            attrs = set(re.findall(r'"([A-Za-z0-9_]+)"\s*:\s*true\b', body))
            if attrs:
                allowlists[rule_name] = attrs
        if "fx_component" in allowlists:
            allowlists["fx_test_component"] = allowlists["fx_component"] | {"test_type"}
        return allowlists

    # Check whether a Starlark statement or keyword argument carries # @bazel2gn:skip.
    def _has_bazel2gn_skip_on_node(file_lines: list[str], node) -> bool:
        start = max(1, getattr(node, "lineno", 1))
        end = max(start, getattr(node, "end_lineno", start))
        check_indices = range(max(0, start - 2), min(len(file_lines), end))
        return any("@bazel2gn:skip" in file_lines[i] for i in check_indices)

    def check_unsupported_fx_component_attrs(tree_node):
        """Reject unsupported symbolic-macro and bazel2gn attributes on component rules."""
        bz_full = os.path.join(workdir, rel_path)
        try:
            src_lines = open(bz_full, encoding="utf-8").read().splitlines()
        except OSError:
            src_lines = []
        gn_rel = f"{pkg_dir}/BUILD.gn" if pkg_dir else "BUILD.gn"
        gn_full = os.path.join(workdir, gn_rel)
        gn_content = ""
        if os.path.isfile(gn_full):
            try:
                gn_content = open(gn_full, encoding="utf-8").read()
            except OSError:
                gn_content = ""
        sentinel_match = re.search(r"(?m)^##\s*BAZEL2GN SENTINEL", gn_content)
        b2gn_allowlists = (
            _load_bazel2gn_component_allowlists() if sentinel_match else {}
        )

        manifest_attrs_by_name: dict[str, set[str]] = {}
        manifest_comp_name_by_name: dict[str, str] = {}
        unskipped_fx_comp_deps: set[str] = set()
        for stmt_n in tree_node.body:
            if not (
                isinstance(stmt_n, ast.Expr)
                and isinstance(stmt_n.value, ast.Call)
                and isinstance(stmt_n.value.func, ast.Name)
            ):
                continue
            c_node = stmt_n.value
            if c_node.func.id == "fx_component" and not _has_bazel2gn_skip_on_node(
                src_lines, stmt_n
            ):
                for kw in c_node.keywords:
                    if kw.arg in ("compiled_manifest", "deps") and not _has_bazel2gn_skip_on_node(
                        src_lines, kw
                    ):
                        for sub in ast.walk(kw.value):
                            if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                                if sub.value.startswith(":"):
                                    unskipped_fx_comp_deps.add(sub.value.lstrip(":"))
            if c_node.func.id not in ("fx_component_manifest", "fx_compiled_manifest"):
                continue
            m_name = ""
            m_comp_name = ""
            kw_names: set[str] = set()
            for kw in c_node.keywords:
                if not kw.arg:
                    continue
                kw_names.add(kw.arg)
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    m_name = kw.value.value
                elif (
                    kw.arg == "component_name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    m_comp_name = kw.value.value
            if m_name:
                manifest_attrs_by_name[m_name] = kw_names
                manifest_comp_name_by_name[m_name] = m_comp_name or m_name

        manifest_only_attrs = FX_COMPONENT_MANIFEST_ONLY_ATTRS
        disabled_macro_attrs = FX_COMPONENT_DISABLED_MACRO_ATTRS
        for stmt_n in tree_node.body:
            if not (
                isinstance(stmt_n, ast.Expr)
                and isinstance(stmt_n.value, ast.Call)
                and isinstance(stmt_n.value.func, ast.Name)
            ):
                continue
            c_node = stmt_n.value
            r_id = c_node.func.id
            stmt_skipped = _has_bazel2gn_skip_on_node(src_lines, stmt_n)
            t_name = ""
            cm_target = ""
            for kw in c_node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    t_name = kw.value.value
                elif (
                    kw.arg == "compiled_manifest"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    cm_target = kw.value.value.lstrip(":")

            # Enforce live bazel2gn allowlists on un-skipped companion manifest/binary rules.
            if (
                r_id in ("fx_component_manifest", "fx_compiled_manifest", "fx_packaged_binary")
                and r_id in b2gn_allowlists
                and not stmt_skipped
                and (r_id == "fx_compiled_manifest" or t_name in unskipped_fx_comp_deps)
            ):
                allowed_set = b2gn_allowlists[r_id]
                for kw in c_node.keywords:
                    if (
                        not kw.arg
                        or kw.arg in allowed_set
                        or _has_bazel2gn_skip_on_node(src_lines, kw)
                    ):
                        continue
                    kw_lineno = getattr(kw, "lineno", getattr(c_node, "lineno", 1))
                    allowed_str = ", ".join(f"`{a}`" for a in sorted(allowed_set))
                    msg = (
                        f"Target '{t_name}' ({r_id}) in '{rel_path}' sets attribute "
                        f"`{kw.arg}`, which `bazel2gn` does not support on `{r_id}` "
                        f"(supported attributes: {allowed_str})."
                    )
                    rem = (
                        f"Remove `{kw.arg}` from `{r_id}(name = \"{t_name}\")` in "
                        f"'{rel_path}' (or move it to the rule that supports it) and "
                        f"re-run `fx bazel2gn -d {pkg_dir}`."
                    )
                    _emit(
                        findings,
                        "unsupported_fx_component_attribute",
                        "error",
                        rel_path,
                        kw_lineno,
                        msg,
                        rem,
                    )

            if r_id not in ("fx_component", "fx_test_component"):
                continue
            b2gn_comp_allowed = (
                b2gn_allowlists.get(r_id) if not stmt_skipped else None
            )
            live_disabled_comp = (
                DISABLED_SYMBOLIC_MACRO_ATTRS.get(r_id, ("", set()))[1]
                | set(disabled_macro_attrs)
            )
            for kw in c_node.keywords:
                kw_lineno = getattr(kw, "lineno", getattr(c_node, "lineno", 1))
                if kw.arg in manifest_only_attrs:
                    attr_name = kw.arg
                    kw_val = (
                        kw.value.value
                        if isinstance(kw.value, ast.Constant)
                        and isinstance(kw.value.value, str)
                        else ""
                    )
                    has_on_manifest = cm_target in manifest_attrs_by_name and (
                        attr_name in manifest_attrs_by_name[cm_target]
                        or (
                            attr_name == "component_name"
                            and bool(kw_val)
                            and manifest_comp_name_by_name.get(cm_target) == kw_val
                        )
                    )
                    msg = (
                        f"Target '{t_name}' ({r_id}) in '{rel_path}' sets "
                        f"`{attr_name}`, which neither `{r_id}` nor `bazel2gn` "
                        f"supports (`Error: no such attribute '{attr_name}' in "
                        f"'{r_id}' macro`; `bazel2gn doesn't support attribute "
                        f'"{attr_name}" of {r_id}`). '
                        "In `//build/bazel/rules/components:fx_component.bzl`, "
                        f"`{attr_name}` is declared on the companion "
                        "`fx_component_manifest` target referenced by "
                        "`compiled_manifest`, never on `fx_component` or "
                        "`fx_test_component` (do not add `# @bazel2gn:skip`)."
                    )
                    if has_on_manifest:
                        rem = (
                            f"Remove `{attr_name}` from `{r_id}(name = \"{t_name}\")` "
                            f"in '{rel_path}' (companion manifest target "
                            f"`:{cm_target}` already provides it) and re-run "
                            f"`fx bazel2gn -d {pkg_dir}` if dual-building."
                        )
                    else:
                        rem = (
                            f"Move `{attr_name}` from `{r_id}(name = \"{t_name}\")` "
                            "to its companion `fx_component_manifest` target "
                            f"(`compiled_manifest`) in '{rel_path}', remove "
                            f"`{attr_name}` from `{r_id}(name = \"{t_name}\")`, and "
                            f"re-run `fx bazel2gn -d {pkg_dir}` if dual-building."
                        )
                    _emit(
                        findings,
                        "unsupported_fx_component_attribute",
                        "error",
                        rel_path,
                        kw_lineno,
                        msg,
                        rem,
                    )
                    continue
                if kw.arg in live_disabled_comp or (
                    r_id == "fx_component" and kw.arg == "test_type"
                ):
                    _emit_disabled_fx_component_attr(r_id, t_name, kw.arg, kw_lineno)
                    continue
                if (
                    b2gn_comp_allowed is not None
                    and kw.arg
                    and kw.arg not in b2gn_comp_allowed
                    and not _has_bazel2gn_skip_on_node(src_lines, kw)
                ):
                    allowed_str = ", ".join(f"`{a}`" for a in sorted(b2gn_comp_allowed))
                    msg = (
                        f"Target '{t_name}' ({r_id}) in '{rel_path}' sets attribute "
                        f"`{kw.arg}`, which `bazel2gn` does not support on `{r_id}` "
                        f"(supported attributes: {allowed_str})."
                    )
                    rem = (
                        f"Remove `{kw.arg}` from `{r_id}(name = \"{t_name}\")` in "
                        f"'{rel_path}' (or move it to the companion "
                        f"`fx_component_manifest` target if it is a manifest attribute) "
                        f"and re-run `fx bazel2gn -d {pkg_dir}`."
                    )
                    _emit(
                        findings,
                        "unsupported_fx_component_attribute",
                        "error",
                        rel_path,
                        kw_lineno,
                        msg,
                        rem,
                    )

        # Also check that dual-built BUILD.gn files do not retain stale component_name
        # attributes on fuchsia_component / fuchsia_test_component blocks with cm_label
        # below ## BAZEL2GN SENTINEL.
        if sentinel_match:
            sentinel_start_line = gn_content[: sentinel_match.start()].count("\n") + 1
            post_sentinel = gn_content[sentinel_match.start() :]
            comp_block_re = re.compile(
                r'\b(fuchsia_(?:test_)?component)\(\s*"([^"]+)"\s*\)\s*\{([^}]*)\}',
                re.DOTALL,
            )
            for m in comp_block_re.finditer(post_sentinel):
                g_rule, g_name, g_body = m.group(1), m.group(2), m.group(3)
                if "cm_label" in g_body and re.search(
                    r"(?m)^\s*component_name\s*=", g_body
                ):
                    line_no = (
                        sentinel_start_line
                        + post_sentinel[: m.start()].count("\n")
                    )
                    msg = (
                        f"Target '{g_name}' ({g_rule}) below `## BAZEL2GN SENTINEL` "
                        f"in '{gn_rel}' sets both `cm_label` and `component_name`. "
                        "`bazel2gn` sets `component_name` only on "
                        "`fuchsia_component_manifest`, never on `fuchsia_component` "
                        "or `fuchsia_test_component`."
                    )
                    rem = (
                        f"Ensure `component_name` is removed from the corresponding "
                        f"`fx_component` / `fx_test_component` in '{rel_path}' and "
                        f"re-run `fx bazel2gn -d {pkg_dir}` to regenerate '{gn_rel}'."
                    )
                    _emit(
                        findings,
                        "unsupported_fx_component_attribute",
                        "error",
                        gn_rel,
                        line_no,
                        msg,
                        rem,
                    )

    # Check that BUILD.bazel targets do not pass attributes disabled (`None`) in symbolic macros.
    def check_disabled_symbolic_macro_attrs(tree_node):
        if not DISABLED_SYMBOLIC_MACRO_ATTRS:
            return
        for stmt_n in tree_node.body:
            if not (
                isinstance(stmt_n, ast.Expr)
                and isinstance(stmt_n.value, ast.Call)
                and isinstance(stmt_n.value.func, ast.Name)
            ):
                continue
            c_node = stmt_n.value
            m_name = c_node.func.id
            if m_name in ("fx_component", "fx_test_component"):
                continue
            entry = DISABLED_SYMBOLIC_MACRO_ATTRS.get(m_name)
            if not entry:
                continue
            bzl_lbl, none_attrs = entry
            t_name = ""
            for kw in c_node.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    t_name = kw.value.value
                    break
            for kw in c_node.keywords:
                if not kw.arg or kw.arg not in none_attrs:
                    continue
                kw_lineno = getattr(kw, "lineno", getattr(c_node, "lineno", 1))
                msg = (
                    f"Target '{t_name}' ({m_name}) in '{rel_path}' sets attribute "
                    f"`{kw.arg}`, which is explicitly disabled (`None`) in `{bzl_lbl}` "
                    f"and fails package loading (`Error: no such attribute '{kw.arg}' "
                    f"in '{m_name}' macro`)."
                )
                rem = (
                    f"Remove `{kw.arg}` from `{m_name}(name = \"{t_name}\")` in "
                    f"'{rel_path}' and re-run `fx bazel2gn -d {pkg_dir}` if "
                    "dual-building."
                )
                _emit(
                    findings,
                    "unsupported_symbolic_macro_attribute",
                    "error",
                    rel_path,
                    kw_lineno,
                    msg,
                    rem,
                )

    check_single_use_private_vars(tree)
    check_wrap_host_rust_test_naming(tree, base_gn_text)
    check_unnecessary_test_check_cfg_flags(tree)
    check_repeated_deps_select(tree)
    check_redundant_forwarding_alias(tree, base_gn_text)
    check_commented_out_deps(tree)
    check_host_constraints_parity(tree, base_gn_text)
    check_host_tool_transitive_constraints(tree)
    check_fuchsia_only_rust_bin_and_test_attrs(tree, base_gn_text)
    check_unsupported_fx_component_attrs(tree)
    check_disabled_symbolic_macro_attrs(tree)

    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        rule_name = ""
        if isinstance(node.func, ast.Name):
            rule_name = node.func.id
        elif isinstance(node.func, ast.Attribute):
            rule_name = node.func.attr
        if not rule_name:
            continue

        kw_map = {}
        for kw in node.keywords:
            if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                kw_map[kw.arg] = (kw.value.value, kw.lineno)

        target_name, _ = kw_map.get("name", ("", 0))

        # 0. Enforce Fuchsia-specific wrapper macros (fx_cc_*, rustc_*) instead of raw
        #    @rules_cc// or @rules_rust// rules per the Fuchsia Bazel style guide.
        UNWRAPPED_FUCHSIA_RULES = {
            "cc_library": (
                "fx_cc_library",
                "//build/bazel/rules/cc:fx_cc_library.bzl",
                "build/bazel/rules/cc/fx_cc_library.bzl",
            ),
            "cc_binary": (
                "fx_cc_binary",
                "//build/bazel/rules/cc:fx_cc_binary.bzl",
                "build/bazel/rules/cc/fx_cc_binary.bzl",
            ),
            "rust_library": (
                "rustc_library",
                "//build/bazel/rules/rust:rustc_library.bzl",
                "build/bazel/rules/rust/rustc_library.bzl",
            ),
            "rust_binary": (
                "rustc_binary",
                "//build/bazel/rules/rust:rustc_binary.bzl",
                "build/bazel/rules/rust/rustc_binary.bzl",
            ),
            "rust_proc_macro": (
                "rustc_proc_macro",
                "//build/bazel/rules/rust:rustc_proc_macro.bzl",
                "build/bazel/rules/rust/rustc_proc_macro.bzl",
            ),
            "rust_test": (
                "rustc_test",
                "//build/bazel/rules/rust:rustc_test.bzl",
                "build/bazel/rules/rust/rustc_test.bzl",
            ),
            "rust_shared_library": (
                "rustc_dylib",
                "//build/bazel/rules/rust:rustc_dylib.bzl",
                "build/bazel/rules/rust/rustc_dylib.bzl",
            ),
            "rust_dylib_library": (
                "rustc_dylib",
                "//build/bazel/rules/rust:rustc_dylib.bzl",
                "build/bazel/rules/rust/rustc_dylib.bzl",
            ),
        }
        if (
            rule_name in UNWRAPPED_FUCHSIA_RULES
            and not rel_path.startswith("build/bazel/")
            and not is_preexisting_target_attr(rule_name, target_name)
        ):
            wrapper_rule, wrapper_bzl, wrapper_path = UNWRAPPED_FUCHSIA_RULES[rule_name]
            if os.path.isfile(os.path.join(workdir, wrapper_path)):
                pkg_d = os.path.dirname(rel_path).strip("/")
                _emit(findings, "unwrapped_external_language_rule", "error", rel_path, getattr(node, "lineno", 1), f"Target '{target_name}' in '{rel_path}' uses unwrapped `{rule_name}()` "
                        f"instead of the Fuchsia-specific wrapper `{wrapper_rule}()` from "
                        f"`{wrapper_bzl}` (see "
                        "https://fuchsia.dev/fuchsia-src/development/languages/bazel/style/common#use_fuchsia-specific_wrappers).", f"Replace `load(..., \"{rule_name}\")` and `{rule_name}(...)` in "
                        f"'{rel_path}' with `load(\"{wrapper_bzl}\", \"{wrapper_rule}\")` and "
                        f"`{wrapper_rule}(...)`, then re-run `fx bazel2gn -d {pkg_d}` if dual-building.")

        # 1. Redundant crate_name matching name (or name with '-' -> '_').
        # On rustc_binary, bazel2gn maps crate_name to GN output_name, so crate_name is intentional when output_name is needed.
        check_rustc_test_crate_reuse(node, rule_name, target_name, kw_map)
        check_rustc_test_raw_output_name(node, rule_name, target_name)
        if "crate_name" in kw_map and target_name and rule_name not in ("rustc_binary", "rustc_test"):
            c_name, lineno = kw_map["crate_name"]
            if (
                c_name == target_name or c_name == target_name.replace("-", "_")
            ) and not is_preexisting_target_attr(
                rule_name, target_name, f'crate_name="{c_name}"'
            ):
                _emit(findings, "redundant_default_attribute", "error", rel_path, lineno, f"Target '{target_name}' ({rule_name}) specifies redundant `crate_name = \"{c_name}\"`, "
                        "which is already the default derived from `name`.", "Remove `crate_name` from BUILD.bazel and re-run `fx bazel2gn` so the target definition stays minimal.")

        # 2. Redundant version = "0.1.0" (or placeholder negative version like "-1.1.0") on rustc_* targets
        if rule_name.startswith("rustc_") and "version" in kw_map:
            ver, lineno = kw_map["version"]
            if (
                ver in ("0.1.0", "0.0.1") or ver.startswith("-")
            ) and not is_preexisting_target_attr(
                rule_name, target_name, f'version="{ver}"'
            ):
                _emit(findings, "redundant_default_attribute", "error", rel_path, lineno, f"Target '{target_name}' ({rule_name}) specifies redundant or placeholder `version = \"{ver}\"`, "
                        "which defaults to \"0.1.0\" in Fuchsia's Rust Bazel rules and is not emitted by bazel2gn.", "Remove `version` from BUILD.bazel (and from BUILD.gn if dual-building) and re-run `fx bazel2gn`.")

        # 3. Redundant default crate_root on rustc_library / rustc_binary
        if "crate_root" in kw_map:
            c_root, lineno = kw_map["crate_root"]
            if (
                (rule_name == "rustc_library" and c_root == "src/lib.rs")
                or (rule_name == "rustc_binary" and c_root == "src/main.rs")
            ) and not is_preexisting_target_attr(
                rule_name, target_name, f'crate_root="{c_root}"'
            ):
                _emit(findings, "redundant_default_attribute", "error", rel_path, lineno, f"Target '{target_name}' ({rule_name}) specifies redundant `crate_root = \"{c_root}\"`, "
                        f"which is the default for `{rule_name}`.", "Remove `crate_root` from BUILD.bazel and re-run `fx bazel2gn`.")

        # 3a. Redundant default lint_config on Fuchsia Rust wrapper macros
        lint_info = _redundant_rust_lint_config_info(node, rule_name, pkg_dir)
        if lint_info is not None:
            is_preexisting_lint = (
                (rule_name, target_name) in base_redundant_lint_targets
                if target_name
                else bool(
                    base_bazel_flat
                    and re.sub(r"\s+", "", ast.get_source_segment(source, node) or "")
                    in base_bazel_flat
                )
            )
            if not is_preexisting_lint:
                l_cfg, lineno, is_testonly_target = lint_info
                disp_name = target_name or "<unnamed>"
                _emit(
                    findings,
                    "redundant_default_attribute",
                    "error",
                    rel_path,
                    lineno,
                    f"Target '{disp_name}' ({rule_name}) specifies redundant "
                    f'`lint_config = "{l_cfg}"`, which `{rule_name}` already selects by default'
                    + (
                        " when `testonly = True`."
                        if is_testonly_target and rule_name not in ("rustc_test", "host_rustc_test")
                        else "."
                    ),
                    "Remove `lint_config` (and any accompanying `# @bazel2gn:skip` or workaround comment) "
                    "from BUILD.bazel and re-run `fx bazel2gn` if dual-building.",
                )

        # 3b. Redundant output_name / gn_output_name / out matching name (or unsupported output_name on go_binary / fx_cc_*)
        for out_attr in ("output_name", "gn_output_name", "out"):
            if out_attr not in kw_map:
                continue
            out_val, lineno = kw_map[out_attr]
            if is_preexisting_target_attr(
                rule_name, target_name, f'{out_attr}="{out_val}"'
            ):
                continue
            if target_name and out_val == target_name:
                _emit(findings, "redundant_default_attribute", "error", rel_path, lineno, f"Target '{target_name}' ({rule_name}) specifies redundant `{out_attr} = \"{out_val}\"`, "
                        "which is already the default derived from `name` (and `output_name` is not a valid attribute on "
                        "`go_binary` / `go_binary_host_tool`).", f"Remove `{out_attr}` from BUILD.bazel (and from BUILD.gn if dual-building) and re-run `fx bazel2gn`.")
            elif out_attr == "output_name" and rule_name in ("go_binary", "go_binary_host_tool"):
                _emit(findings, "redundant_default_attribute", "error", rel_path, lineno, f"Target '{target_name}' ({rule_name}) specifies `output_name = \"{out_val}\"`, "
                        f"which is not a valid attribute on `{rule_name}` (`go_binary` uses `out` when a custom binary name is needed).", f"Omit `output_name` when it matches `name`, or use `out = \"{out_val}\"` if a custom binary name is required, "
                        "without modifying shared macros in `//build/bazel/rules/host:defs.bzl`.")
            elif out_attr == "output_name" and rule_name in ("fx_cc_binary", "fx_cc_test_binary", "fx_cc_test"):
                _emit(findings, "unsupported_cc_output_name_attribute", "error", rel_path, lineno, f"Target '{target_name}' ({rule_name}) specifies `output_name = \"{out_val}\"`, "
                        f"which is not a valid attribute on `{rule_name}`; use `gn_output_name = \"{out_val}\"` instead.", f"Replace `output_name = \"{out_val}\"` with `gn_output_name = \"{out_val}\"` on `{target_name}` in '{rel_path}' and re-run `fx bazel2gn`.")

        # 3c. Legacy host-tool macros (go_binary_host_tool, py_binary_host_tool) require
        #     caller package visibility on //build/bazel/versioning:is_api_level_PLATFORM.
        if rule_name in ("go_binary_host_tool", "py_binary_host_tool"):
            pkg_d = os.path.dirname(rel_path).strip("/")
            if pkg_d and not is_covered_by_platform_api_vis(pkg_d):
                _emit(findings, "missing_platform_api_level_visibility", "error", rel_path, getattr(node, "lineno", 1), f"Target '{target_name}' ({rule_name}) invokes legacy macro `{rule_name}`, which "
                        "expands `//build/bazel/versioning:is_api_level_PLATFORM` in the calling package, "
                        f"but 'build/bazel/versioning/BUILD.bazel' does not grant visibility to '//{pkg_d}'.", f"Add `\"//{pkg_d}:__pkg__\"` (in alphabetical order) to the `visibility` list of "
                        "`is_api_level_PLATFORM` in 'build/bazel/versioning/BUILD.bazel'. Never modify "
                        "'build/bazel/rules/host/defs.bzl' to work around this visibility requirement.")

        # 4. Platform/CPU-specific target using srcs = select({cond: [...], "//conditions:default": []})
        #    instead of target_compatible_with = [cond].
        for kw in node.keywords:
            if kw.arg != "srcs":
                continue
            val = kw.value
            if (
                isinstance(val, ast.Call)
                and isinstance(val.func, ast.Name)
                and val.func.id == "select"
                and val.args
                and isinstance(val.args[0], ast.Dict)
            ):
                kw_seg = ast.get_source_segment(source, kw) or ""
                if is_preexisting_target_attr(rule_name, target_name, kw_seg):
                    continue
                d = val.args[0]
                non_default_keys = []
                has_empty_default = False
                for k_node, v_node in zip(d.keys, d.values):
                    if isinstance(k_node, ast.Constant) and isinstance(k_node.value, str):
                        if k_node.value == "//conditions:default":
                            if isinstance(v_node, ast.List) and len(v_node.elts) == 0:
                                has_empty_default = True
                        else:
                            non_default_keys.append(k_node.value)
                if has_empty_default and len(non_default_keys) == 1:
                    cond = non_default_keys[0]
                    _emit(findings, "select_srcs_instead_of_target_compatible_with", "error", rel_path, kw.lineno, f"Target '{target_name}' ({rule_name}) gates all `srcs` behind "
                            f"`select({{\"{cond}\": [...], \"//conditions:default\": []}})` "
                            "instead of restricting the target with `target_compatible_with`.", f"Set `srcs` directly to the source list on '{target_name}', replace the "
                            f"`select()` + `# @bazel2gn:raw_overwrite` workaround with "
                            f"`target_compatible_with = [\"{cond}\"]`, and re-run `fx bazel2gn`.")


        if rule_name == "build_flags" and "test" in target_name.lower():
            has_testonly = any(
                kw.arg == "testonly" and isinstance(kw.value, ast.Constant) and kw.value.value is True
                for kw in node.keywords
            )
            if not has_testonly:
                _emit(
                    findings,
                    "missing_testonly_on_test_build_flags",
                    "error",
                    rel_path,
                    getattr(node, "lineno", 1),
                    f"`build_flags` target '{target_name}' appears to mirror a test-specific GN config but is missing `testonly = True`.",
                    f"Add `testonly = True` to `build_flags(name = \"{target_name}\", ...)` in '{rel_path}'."
                )

        # 6. genrule commands added or changed by the migration build paths and arguments from
        #    Bazel's predefined variables and Starlark values, not from shell command substitution
        #    at execution time (e.g. `$$(dirname $(location <one input>))`, `$$(python3 -c ...)`)
        #    or hard-coded output-tree paths.
        if rule_name == "genrule":
            for kw in node.keywords:
                if kw.arg not in ("cmd", "cmd_bash"):
                    continue
                segment = ast.get_source_segment(source, kw.value) or ""
                if segment and re.sub(r"\s+", "", segment) in base_bazel_flat:
                    continue
                cmd_text = " ".join(
                    n.value
                    for n in ast.walk(kw.value)
                    if isinstance(n, ast.Constant) and isinstance(n.value, str)
                )
                problems = [f"`$$({m.group(1)} ...)`" for m in re.finditer(r"\$\$\(\s*([^\s()]*)", cmd_text)]
                if "`" in cmd_text:
                    problems.append("a backtick command substitution")
                problems += [
                    f"a hard-coded `{m.group(1)}/` path"
                    for m in re.finditer(r"\b(bazel-(?:out|bin|genfiles)|execroot)/", cmd_text)
                ]
                problems = list(dict.fromkeys(problems))
                if not problems:
                    continue
                _emit(findings, "genrule_cmd_shell_derived_paths", "error", rel_path, kw.lineno, f"genrule '{target_name}' `{kw.arg}` derives paths or arguments in the shell at "
                        f"execution time or hard-codes output-tree paths: {', '.join(problems)}. Reviewers "
                        "reject this as non-canonical (e.g. the `dirname` of one arbitrary input to get a "
                        "directory), and bazel2gn only converts `$@`/`$<` and literal words to GN action args.", "Build the command from Bazel's predefined genrule variables: outputs `$@`/`$(OUTS)`, "
                        "the output directory `$(@D)`/`$(RULEDIR)` (never `dirname $@` or `bazel-out/...`), "
                        "inputs `$<`/`$(SRCS)`/`$(execpath <label>)`/`$(execpaths <label>)`. A directory of "
                        "checked-in inputs that GN passes as `rebase_path(\"<dir>\", root_build_dir)` is "
                        "`package_name() + \"/<dir>\"` (genrules run from the execution root, where the "
                        "package's sources are under its package path), with a comment saying it holds "
                        "checked-in inputs. A value GN computes at gen time (e.g. a `read_file(\"<f>\", "
                        "\"json\")` list) is written out in BUILD.bazel as a Starlark list wrapped in "
                        "`# LINT.IfChange` / `# LINT.ThenChange(<f>)` and joined into the command "
                        "(`\" \".join(<LIST>)`), not read by a `python3 -c`/`cat`/`jq` subshell. Re-run "
                        "`fx bazel2gn -d <dir>` and `fx bazel build` for the package.")

    # 5. GN read_file("<manifest>", "json") + foreach() expanded into static BUILD.bazel lists
    #    without LINT.IfChange / LINT.ThenChange(<manifest>) when <manifest> remains in the tree.
    pkg_dir = os.path.dirname(rel_path)
    base_gn_lines = git_lines(["show", f"{change_base}:{pkg_dir}/BUILD.gn"])
    base_gn_text = "\n".join(base_gn_lines)
    if base_gn_text and not (pkg_dir not in target_dirs and base_bazel_flat):
        for m in re.finditer(r'\bread_file\(\s*"([^"]+)"\s*,\s*"json"\s*\)', base_gn_text):
            manifest_rel_pkg = m.group(1)
            if manifest_rel_pkg.startswith("//"):
                manifest_repo_rel = manifest_rel_pkg[2:]
            else:
                manifest_repo_rel = os.path.normpath(os.path.join(pkg_dir, manifest_rel_pkg))
            manifest_abs = os.path.join(workdir, manifest_repo_rel)
            if not os.path.isfile(manifest_abs):
                continue
            has_then_change = bool(
                re.search(
                    r"#\s*LINT\.ThenChange\([^)]*" + re.escape(os.path.basename(manifest_rel_pkg)),
                    source,
                )
            )
            if not has_then_change:
                _emit(findings, "missing_ifttt_for_manifest_list", "error", rel_path, 1, f"'{rel_path}' inline-expands a file list that `{pkg_dir}/BUILD.gn` previously "
                        f"loaded via `read_file(\"{manifest_rel_pkg}\", \"json\")`, while "
                        f"'{manifest_repo_rel}' still exists without `# LINT.IfChange` / "
                        f"`# LINT.ThenChange({manifest_rel_pkg})`.", f"Wrap the expanded file list in '{rel_path}' with `# LINT.IfChange` and "
                        f"`# LINT.ThenChange({manifest_rel_pkg})` (or remove '{manifest_repo_rel}' if "
                        "no script or tool uses it anymore).")

    # 7. @bazel2gn:path_overwrite hygiene: reject multi-label overwrites, reject real
    #    Bazel targets (including same-package fidl_library targets for missing
    #    _rust_next bindings) used as placeholder stand-ins for unmigrated GN-only deps,
    #    and require a separate # @bazel2gn:skip filegroup per overwritten path
    #    using a unique name derived from the overwritten path.
    src_lines = source.splitlines()
    cur_gn_text = ""
    try:
        with open(os.path.join(workdir, pkg_dir, "BUILD.gn"), encoding="utf-8") as gf:
            cur_gn_text = gf.read()
    except OSError:
        pass

    local_fgs = {}
    for stmt in tree.body:
        if not (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
            and stmt.value.func.id == "filegroup"
        ):
            continue
        fg_name = ""
        has_srcs = False
        for kw in stmt.value.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                fg_name = kw.value.value
            elif kw.arg == "srcs":
                has_srcs = True
        if fg_name and not has_srcs:
            fg_skipped = False
            idx = stmt.lineno - 2
            while idx >= 0:
                s_line = src_lines[idx].strip()
                if not s_line:
                    idx -= 1
                    continue
                if s_line.startswith("#"):
                    if "@bazel2gn:skip" in s_line:
                        fg_skipped = True
                    idx -= 1
                    continue
                break
            if not fg_skipped and 0 < stmt.lineno <= len(src_lines) and "@bazel2gn:skip" in src_lines[stmt.lineno - 1]:
                fg_skipped = True
            local_fgs[fg_name] = (stmt.lineno, fg_skipped)

    for fg_name, (fg_line, fg_skipped) in local_fgs.items():
        if is_preexisting_target_attr("filegroup", fg_name):
            continue
        if not fg_skipped:
            _emit(findings, "path_overwrite_placeholder_hygiene", "error", rel_path, fg_line, f"Placeholder target `filegroup(name = \"{fg_name}\")` in '{rel_path}' lacks `# @bazel2gn:skip`.", f"Add `# @bazel2gn:skip` directly above `filegroup(name = \"{fg_name}\")` in '{rel_path}' "
                    f"and re-run `fx bazel2gn -d {pkg_dir}`.")
        if fg_name.lower() in ("bazel2gn_placeholder", "placeholder"):
            _emit(findings, "path_overwrite_placeholder_hygiene", "error", rel_path, fg_line, f"Placeholder `filegroup(name = \"{fg_name}\")` in '{rel_path}' uses a generic name instead of "
                    "a unique name derived from the overwritten GN dependency path.", "Replace generic placeholder filegroups with one `# @bazel2gn:skip` `filegroup(name = \"<unique_name>\")` "
                    "per overwritten dependency, using a unique name derived from the overwritten path (e.g., "
                    "`board_structured_config`, `fuchsia.driver.framework_rust_next`, `sdk_lib_driver_runtime_rust_fidl`), "
                    f"and re-run `fx bazel2gn -d {pkg_dir}`.")

    def derive_fg_name(gn_label: str) -> str:
        s = gn_label.strip().lstrip("/")
        if s.startswith(":"):
            return s[1:]
        if ":" in s:
            _, t = s.split(":", 1)
            return t
        return s.replace("/", "_")

    # Check whether `# @bazel2gn:path_overwrite:<gl>` on `//<bz_pkg>:<bz_target>` is
    # redundant because `<gl>` in `<bz_pkg>/BUILD.gn` is a pure forwarding group that
    # expands directly to `:<bz_target>` (and `:<bz_target>` is visible to `caller_pkg`).
    def is_redundant_gn_forwarding_group_overwrite(
        caller_pkg: str, bz_pkg: str, bz_target: str, gn_label: str
    ) -> str:
        if gn_label == f"//{bz_pkg}":
            gn_target = os.path.basename(bz_pkg)
        elif gn_label.startswith(f"//{bz_pkg}:"):
            gn_target = gn_label.split(":", 1)[1]
        else:
            return ""
        if not gn_target or gn_target == bz_target:
            return ""
        dep_gn_path = os.path.join(workdir, bz_pkg, "BUILD.gn")
        try:
            with open(dep_gn_path, encoding="utf-8") as df:
                dep_gn_raw = df.read()
        except OSError:
            return ""
        dep_gn_clean = re.sub(r"#[^\n]*", "", dep_gn_raw)
        grp_body = None
        tgt_body = None
        for tm in re.finditer(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{', dep_gn_clean):
            ttype, tname = tm.group(1), tm.group(2)
            depth, idx = 1, tm.end()
            while idx < len(dep_gn_clean) and depth > 0:
                if dep_gn_clean[idx] == "{":
                    depth += 1
                elif dep_gn_clean[idx] == "}":
                    depth -= 1
                idx += 1
            body_slice = dep_gn_clean[tm.end() : idx - 1]
            if tname == gn_target and ttype == "group" and grp_body is None:
                grp_body = body_slice
            elif tname == bz_target and ttype not in ("template", "declare_args", "foreach"):
                tgt_body = body_slice
        if grp_body is None or tgt_body is None:
            return ""
        grp_attrs = set(re.findall(r"(?m)^\s*([A-Za-z_]\w*)\s*[+-]?=", grp_body))
        if not grp_attrs or not (grp_attrs <= {"public_deps", "deps", "visibility", "testonly"}):
            return ""
        forwarded = set()
        for dm in re.finditer(r"(?<![\w.])(?:public_deps|deps)\s*\+?=\s*\[([^\]]*)\]", grp_body):
            for raw_d in re.findall(r'"([^"]+)"', dm.group(1)):
                d = re.sub(r"\(.*\)$", "", raw_d.strip())
                if d.startswith(":"):
                    forwarded.add(f"//{bz_pkg}:{d[1:]}")
                elif d.startswith("//"):
                    p, _, t = d[2:].partition(":")
                    p = p.strip("/")
                    forwarded.add(f"//{p}:{t or os.path.basename(p)}")
        if forwarded != {f"//{bz_pkg}:{bz_target}"}:
            return ""
        vis_matches = list(re.finditer(r"(?<![\w.])visibility\s*\+?=\s*\[([^\]]*)\]", tgt_body))
        if vis_matches:
            vis_entries = [
                v.strip()
                for vm in vis_matches
                for v in re.findall(r'"([^"]+)"', vm.group(1))
            ]
            vis_ok = False
            for ve in vis_entries:
                if ve in ("*", "//*"):
                    vis_ok = True
                    break
                if ve in (":*", f"//{bz_pkg}:*") and caller_pkg == bz_pkg:
                    vis_ok = True
                    break
                if ve.startswith("//") and ve.endswith(":*"):
                    vp = ve[2:-2].strip("/")
                    if caller_pkg == vp:
                        vis_ok = True
                        break
                if ve.startswith("//") and ve.endswith("/*"):
                    vp = ve[2:-2].strip("/")
                    if not vp or caller_pkg == vp or caller_pkg.startswith(vp + "/"):
                        vis_ok = True
                        break
            if not vis_ok:
                return ""
        return gn_target

    # Check for redundant @bazel2gn:path_overwrite / @bazel2gn:raw_overwrite comments on
    # targets that bazel2gn does not translate or on third-party labels already mapped in
    # build/tools/bazel2gn/third_party_target_map.json.
    untranslated_bazel2gn_rules = {
        "cc_test",
        "exports_files",
        "filegroup",
        "fx_package",
        "fx_packaged_binary",
        "fx_test",
        "fx_test_component",
        "fx_unittest_component",
        "host_cc_test",
        "host_py_test",
        "host_shac_test",
        "host_test",
        "test_suite",
        "wrap_host_rust_test",
    }

    # Collect fx_test_component names referenced as ":name" (or "name") from any
    # fx_package(test_components = [...]) in this file; bazel2gn translates those.
    def _packaged_test_component_names(mod) -> set[str]:
        names: set[str] = set()
        for node in ast.walk(mod):
            if not (
                isinstance(node, ast.Call)
                and isinstance(node.func, ast.Name)
                and node.func.id == "fx_package"
            ):
                continue
            for kw in node.keywords:
                if kw.arg != "test_components":
                    continue
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        lbl = sub.value.strip()
                        if lbl.startswith(":"):
                            names.add(lbl[1:])
                        elif ":" not in lbl and "/" not in lbl:
                            names.add(lbl)
        return names

    def check_redundant_bazel2gn_overwrites() -> set[int]:
        tp_map: dict[str, str] = {}
        tp_path = os.path.join(
            workdir, "build/tools/bazel2gn/third_party_target_map.json"
        )
        try:
            with open(tp_path, encoding="utf-8") as tf:
                raw_tp = json.load(tf)
            if isinstance(raw_tp, dict):
                tp_map = {str(k).strip(): str(v).strip() for k, v in raw_tp.items()}
        except OSError:
            pass

        # fx_test_component has no standalone GN equivalent, but bazel2gn DOES translate it
        # (to fuchsia_test_component) when an fx_package in the same file lists it in
        # test_components, so overwrites inside such components are not redundant.
        packaged_test_components = _packaged_test_component_names(tree)

        untranslated_spans: list[tuple[int, int, str, str, bool]] = []
        visibility_spans: list[tuple[int, int, str, str, bool]] = []
        for stmt in getattr(tree, "body", ()):
            if not (
                isinstance(stmt, ast.Expr)
                and isinstance(stmt.value, ast.Call)
                and isinstance(stmt.value.func, ast.Name)
            ):
                continue
            rule_name = stmt.value.func.id
            end_line = getattr(stmt, "end_lineno", stmt.lineno)
            t_name = ""
            vis_kw_node = None
            for kw in stmt.value.keywords:
                if (
                    kw.arg == "name"
                    and isinstance(kw.value, ast.Constant)
                    and isinstance(kw.value.value, str)
                ):
                    t_name = kw.value.value
                elif kw.arg == "visibility":
                    vis_kw_node = kw
            is_skipped = False
            idx = stmt.lineno - 2
            while idx >= 0:
                s_line = src_lines[idx].strip()
                if not s_line:
                    idx -= 1
                    continue
                if s_line.startswith("#"):
                    if "@bazel2gn:skip" in s_line:
                        is_skipped = True
                    idx -= 1
                    continue
                break
            is_packaged_test_component = (
                rule_name == "fx_test_component" and t_name in packaged_test_components
            )
            if (
                rule_name in untranslated_bazel2gn_rules and not is_packaged_test_component
            ) or is_skipped:
                untranslated_spans.append(
                    (stmt.lineno, end_line, rule_name, t_name, is_skipped)
                )
            elif vis_kw_node is not None:
                vis_end = getattr(vis_kw_node.value, "end_lineno", vis_kw_node.lineno)
                is_empty_vis = (
                    isinstance(vis_kw_node.value, ast.List)
                    and len(vis_kw_node.value.elts) == 0
                )
                visibility_spans.append(
                    (vis_kw_node.lineno, vis_end, rule_name, t_name, is_empty_vis)
                )

        flagged: set[int] = set()
        for lno, ltxt in enumerate(src_lines, 1):
            ov_m = re.search(
                r"#\s*@bazel2gn:(path_overwrite|raw_overwrite):(.+)$", ltxt
            )
            if not ov_m:
                continue
            if base_bazel_flat and re.sub(r"\s+", "", ltxt) in base_bazel_flat:
                continue
            directive = ov_m.group(1)
            gn_spec_val = ov_m.group(2).strip()
            vis_enclosing = (
                next(
                    (sp for sp in visibility_spans if sp[0] <= lno <= sp[1]),
                    None,
                )
                if directive == "raw_overwrite"
                else None
            )
            if vis_enclosing is not None and not vis_enclosing[4]:
                _, _, v_rule, v_tname, _ = vis_enclosing
                vis_pats = [p.strip() for p in re.findall(r'"([^"]+)"', gn_spec_val)]
                if (
                    vis_pats
                    and "*" not in vis_pats
                    and "//*" not in vis_pats
                    and not any(p.startswith("//build/sdk:") for p in vis_pats)
                    and all(
                        p in (":*", "./*")
                        or bool(re.match(r"^//[^:*]+(?::\*|/\*)$", p))
                        for p in vis_pats
                    )
                ):
                    flagged.add(lno)
                    bz_equiv: list[str] = []
                    for vp in vis_pats:
                        if vp == ":*":
                            continue
                        if vp == "./*":
                            bz_equiv.append(":__subpackages__")
                        elif vp.endswith(":*"):
                            bz_equiv.append(vp[:-2] + ":__pkg__")
                        elif vp.endswith("/*"):
                            bz_equiv.append(vp[:-2] + ":__subpackages__")
                    if not bz_equiv:
                        vis_rem = (
                            f"Remove `# @bazel2gn:raw_overwrite:{gn_spec_val}` on line {lno} "
                            f"of '{rel_path}'. If `{v_rule}(name = \"{v_tname}\")` needs "
                            '`visibility = [ ":*" ]` in BUILD.gn, set '
                            '`visibility = ["//visibility:private"]` in BUILD.bazel (which '
                            "`bazel2gn` converts to `[ \":*\" ]`), or omit `visibility` if "
                            f"GN does not need `:*`, then re-run `fx bazel2gn -d {pkg_dir}`."
                        )
                    else:
                        vis_rem = (
                            f"Remove `# @bazel2gn:raw_overwrite:{gn_spec_val}` on line {lno} "
                            f"of '{rel_path}' and set `visibility = {json.dumps(bz_equiv)}` "
                            f"directly on `{v_rule}(name = \"{v_tname}\")` (`bazel2gn` "
                            'automatically adds `":*"` to non-empty visibility lists and '
                            "translates `:__pkg__` / `:__subpackages__` to `:*` / `/*`), "
                            f"then re-run `fx bazel2gn -d {pkg_dir}`."
                        )
                    _emit(findings, "redundant_bazel2gn_overwrite", "error", rel_path, lno, f"Line {lno} of '{rel_path}' uses unnecessary "
                            f"`# @bazel2gn:raw_overwrite:{gn_spec_val}` on `visibility` of "
                            f'`{v_rule}(name = "{v_tname}")`. `bazel2gn` automatically adds '
                            '`":*"` to non-empty visibility lists, maps '
                            '`"//visibility:private"` to `[ ":*" ]`, and converts '
                            "`:__pkg__` / `:__subpackages__` entries directly to GN `:*` / `/*`.", vis_rem)
                    continue
            po_match = re.match(
                r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:path_overwrite:(.+)$', ltxt
            )
            bz_lbl = po_match.group(1).strip() if po_match else ""
            gn_lbls = [
                tok.strip().strip('"')
                for tok in gn_spec_val.split('", "')
                if tok.strip().strip('"')
            ]
            mapped_gn = tp_map.get(bz_lbl, "") if bz_lbl else ""
            is_tp_mapped = (
                directive == "path_overwrite"
                and len(gn_lbls) == 1
                and bool(mapped_gn)
                and gn_lbls[0] == mapped_gn
            )
            enclosing = next(
                (sp for sp in untranslated_spans if sp[0] <= lno <= sp[1]),
                None,
            )
            if enclosing is None and not is_tp_mapped:
                continue
            if is_tp_mapped and pkg_dir in target_dirs:
                # In target dirs, cc_header_deps_and_annotations
                # redundant_builtin_label_directive already reports a new directive that
                # overrides a third_party_target_map.json label (also inside untranslated or
                # skipped targets, where the remediation is the same: remove the directive);
                # only keep the downstream path_overwrite suppression here so the line is not
                # reported twice.
                flagged.add(lno)
                continue

            reasons: list[str] = []
            if enclosing is not None:
                _, _, r_name, t_name, skipped = enclosing
                tgt_desc = (
                    f'`{r_name}(name = "{t_name}")`' if t_name else f"`{r_name}(...)`"
                )
                if skipped:
                    reasons.append(f"{tgt_desc} is marked `# @bazel2gn:skip`")
                else:
                    reasons.append(f"`bazel2gn` does not translate `{r_name}` targets")
            if is_tp_mapped:
                reasons.append(
                    "`build/tools/bazel2gn/third_party_target_map.json` already maps "
                    f'`"{bz_lbl}"` to `"{mapped_gn}"`'
                )

            flagged.add(lno)
            _emit(findings, "redundant_bazel2gn_overwrite", "error", rel_path, lno, f"Line {lno} of '{rel_path}' has a redundant "
                    f"`# @bazel2gn:{directive}:{gn_spec_val}` annotation because "
                    + " and ".join(reasons)
                    + ".", f"Remove the `# @bazel2gn:{directive}:{gn_spec_val}` comment on "
                    f"line {lno} of '{rel_path}', re-run `fx bazel2gn -d {pkg_dir}` (which "
                    "translates mapped external repository labels to the exact GN target "
                    "automatically), and set `disputed_findings` to `[]`.")
        return flagged

    redundant_overwrite_lines = check_redundant_bazel2gn_overwrites()

    for lineno, line_txt in enumerate(src_lines, 1):
        if lineno in redundant_overwrite_lines:
            continue
        po_m = re.match(r'^\s*"([^"]+)"\s*,\s*#\s*@bazel2gn:path_overwrite:(.+)$', line_txt)
        if not po_m:
            continue
        if base_bazel_flat and re.sub(r"\s+", "", line_txt) in base_bazel_flat:
            continue
        bz_label = po_m.group(1).strip()
        gn_spec = po_m.group(2).strip()
        if (
            "$" in gn_spec
            or bz_label.startswith(("@", "//third_party/", "//:", "//src/zircon/lib/zircon"))
            or "//third_party/" in gn_spec
        ):
            continue
        gn_labels = [tok.strip().strip('"') for tok in gn_spec.split('", "') if tok.strip().strip('"')]
        if len(gn_labels) > 1:
            suggested = ", ".join(
                f'`":{derive_fg_name(gl)}",  # @bazel2gn:path_overwrite:{gl}`' for gl in gn_labels
            )
            _emit(findings, "path_overwrite_placeholder_hygiene", "error", rel_path, lineno, f"Line {lineno} of '{rel_path}' packs multiple GN labels into a single `# @bazel2gn:path_overwrite:` "
                    f"comment (`{gn_spec}`). Each overwritten dependency must have its own list entry and its own "
                    "`# @bazel2gn:skip` `filegroup` with a unique name derived from the overwritten path.", f"Split line {lineno} into one entry per overwritten path ({suggested}), define a matching "
                    f"`# @bazel2gn:skip` `filegroup(name = \"...\")` for each in '{rel_path}', and re-run "
                    f"`fx bazel2gn -d {pkg_dir}`.")
            continue
        if not gn_labels:
            continue
        gl = gn_labels[0]
        if bz_label.startswith("//"):
            bz_pkg, has_colon, bz_target = bz_label[2:].partition(":")
            bz_pkg = bz_pkg.strip("/")
            bz_target = bz_target or os.path.basename(bz_pkg)
            # Legitimate real-target overwrites:
            # 1) Explicit target in the same package when GN uses the package-default label //<pkg>
            #    and //<pkg> is not a pure forwarding group to :<bz_target>
            # 2) Same-package C/C++ library for a GN-only *_static wrapper (not a missing FIDL *_rust_next binding)
            # 3) Package-local forwarding group("...") in BUILD.gn that forwards to //<bz_pkg>
            # 4) A cc_link_stub_from_ifs_file target in //<bz_pkg> whose ifs_file lives in
            #    the overwritten GN target's package
            def _is_ifs_link_stub_for_gn_label(stub_pkg: str, stub_name: str, gn_lbl: str) -> bool:
                if not gn_lbl.startswith("//"):
                    return False
                gn_pkg = gn_lbl[2:].split(":", 1)[0].strip("/")
                if not gn_pkg:
                    return False
                stub_bz = os.path.join(workdir, stub_pkg, "BUILD.bazel")
                if not os.path.isfile(stub_bz):
                    return False
                try:
                    with open(stub_bz, encoding="utf-8") as sf:
                        stub_txt = sf.read()
                except OSError:
                    return False
                return (
                    "cc_link_stub_from_ifs_file" in stub_txt
                    and f'name = "{stub_name}"' in stub_txt
                    and f'"//{gn_pkg}:' in stub_txt
                )

            is_missing_fidl_binding = bool(re.search(r":.+_(?:rust_next|rust|cpp|hlcpp|banjo)\b", gl))
            related = False
            if gl == f"//{bz_pkg}" and has_colon:
                related = True
            elif gl.startswith(f"//{bz_pkg}:") and not is_missing_fidl_binding:
                related = True
            elif _is_ifs_link_stub_for_gn_label(bz_pkg, bz_target, gl):
                related = True
            elif gl.startswith(":"):
                gn_local = gl[1:]
                if cur_gn_text and re.search(
                    r'\bgroup\(\s*"' + re.escape(gn_local) + r'"\s*\)\s*\{[^}]*//' + re.escape(bz_pkg),
                    cur_gn_text,
                    re.S,
                ):
                    related = True
            if related and (gl == f"//{bz_pkg}" or gl.startswith(f"//{bz_pkg}:")):
                if not (
                    pkg_dir not in target_dirs
                    and base_bazel_flat
                    and re.sub(r"\s+", "", line_txt) in base_bazel_flat
                ):
                    fwd_grp = is_redundant_gn_forwarding_group_overwrite(
                        pkg_dir, bz_pkg, bz_target, gl
                    )
                    if fwd_grp:
                        _emit(findings, "redundant_forwarding_group_path_overwrite", "error", rel_path, lineno, f"Line {lineno} of '{rel_path}' annotates `\"{bz_label}\"` with "
                                f"`# @bazel2gn:path_overwrite:{gl}`, but in '{bz_pkg}/BUILD.gn' "
                                f"`group(\"{fwd_grp}\")` is a pure forwarding group that expands to "
                                f"`//{bz_pkg}:{bz_target}`, so GN can depend on `\"{bz_label}\"` "
                                "directly without this annotation.", f"Remove `# @bazel2gn:path_overwrite:{gl}` from line {lineno} of "
                                f"'{rel_path}' and re-run `fx bazel2gn -d {pkg_dir}`.")
            if not related:
                sug_name = derive_fg_name(gl)
                _emit(findings, "unrelated_path_overwrite_target", "error", rel_path, lineno, f"Line {lineno} of '{rel_path}' uses real Bazel target `\"{bz_label}\"` as a stand-in for "
                        f"`# @bazel2gn:path_overwrite:{gl}`. Unmigrated dependencies in `@bazel2gn:path_overwrite` "
                        "(including missing FIDL bindings such as `*_rust_next`) must use a local `# @bazel2gn:skip` "
                        "`filegroup` with a unique name derived from the overwritten path.", f"Replace `\"{bz_label}\"` on line {lineno} of '{rel_path}' with `\":{sug_name}\"` "
                        f"(`\":{sug_name}\",  # @bazel2gn:path_overwrite:{gl}`), define `# @bazel2gn:skip` "
                        f"`filegroup(name = \"{sug_name}\")` in '{rel_path}', and re-run `fx bazel2gn -d {pkg_dir}`.")
        elif bz_label.startswith(":"):
            fg_ref = bz_label[1:]
            if fg_ref in local_fgs:
                ctx_start = max(0, lineno - 20)
                ctx_comment = "\n".join(
                    s for s in src_lines[ctx_start:lineno] if "#" in s
                ).lower()
                if not any(w in ctx_comment for w in ("placeholder", "filegroup", "todo", "remove")):
                    _emit(findings, "path_overwrite_placeholder_hygiene", "error", rel_path, lineno, f"Placeholder overwrite `\"{bz_label}\"` on line {lineno} of '{rel_path}' lacks a comment "
                            "explaining why the placeholder filegroups are needed and that they should be removed once "
                            "the real Bazel targets exist.", f"Add a comment (with `TODO(...)`) above the placeholder overwrites in '{rel_path}' "
                            "explaining which GN-only targets lack a Bazel build and noting that the placeholder "
                            "filegroups should be removed once they are available.")



# 4. Starlark formatting and buildifier_lint hygiene (including .bzl module-docstring).
def is_starlark_file(rel: str) -> bool:
    if rel.startswith("third_party/") and not rel.startswith(RUST_CRATES + "compat/"):
        return False
    base = os.path.basename(rel)
    return base in ("BUILD.bazel", "BUILD", "MODULE.bazel") or base.endswith((".bzl", ".bazel", ".star"))


starlark_rels = set()
for p in changed:
    norm = p.strip("/")
    if is_starlark_file(norm) and os.path.isfile(os.path.join(workdir, norm)):
        starlark_rels.add(norm)

for td in target_dirs:
    td_abs = os.path.join(workdir, td)
    if not os.path.isdir(td_abs):
        continue
    for root, dirs, files in os.walk(td_abs):
        rel_root = os.path.relpath(root, workdir).strip(".")
        if rel_root != td:
            # Stay within the Bazel package: do not descend into subpackages with their own BUILD file.
            if any(b in files for b in ("BUILD.bazel", "BUILD", "BUILD.gn")):
                dirs[:] = []
                continue
        for fname in files:
            rel = os.path.join(rel_root, fname) if rel_root else fname
            if is_starlark_file(rel):
                starlark_rels.add(rel)


# 4a-0. Reject hand-edits to crate_universe-generated BUILD.bazel files.
def check_generated_vendor_build_hygiene():
    """Reject manual copyright header or package default_visibility edits in crate_universe BUILD.bazel files."""
    copyright_re = re.compile(r"^#\s*Copyright\s+\d{4}\s+The\s+Fuchsia\s+Authors\b", re.M)
    for p in sorted(changed):
        norm = p.strip("/")
        if not (
            norm.startswith(RUST_CRATES + "vendor/")
            and os.path.basename(norm) in ("BUILD.bazel", "BUILD")
        ):
            continue
        full = os.path.join(workdir, norm)
        if not os.path.isfile(full):
            continue
        try:
            with open(full, "r", encoding="utf-8") as f:
                cur_txt = f.read()
        except OSError:
            continue
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{norm}"]))
        has_manual_copyright = bool(
            copyright_re.search(cur_txt[:512])
            and not copyright_re.search(base_txt[:512])
        )
        dropped_default_vis = (
            "default_visibility" in base_txt and "default_visibility" not in cur_txt
        )
        if has_manual_copyright or dropped_default_vis:
            _emit(findings, "hand_edited_generated_vendor_build", "error", norm, 1, f"Auto-generated crate_universe file '{norm}' was hand-edited to "
                    "add a Fuchsia copyright header or remove package-level "
                    "`default_visibility`.", f"Do not hand-edit '{norm}' for copyright headers or visibility. "
                    "Restore the standard crate_universe header and "
                    "`default_visibility = [\"//visibility:public\"]` (or regenerate "
                    "with `fx update-rustc-third-party` from "
                    "`build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl`).")



# Check for newly exported raw .shard.cml files in exports_files() or multiple raw .shard.cml
# includes from the same package in fx_component_manifest(includes) where cml_shard should be used.
def check_cml_shard_targets(starlark_files):
    exempt_prefixes = ("src/sys/test_runners/", "sdk/lib/sys/testing/")
    for rel in sorted(starlark_files):
        if not os.path.basename(rel).startswith("BUILD"):
            continue
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                st_src = f.read()
            st_tree = ast.parse(st_src, filename=rel)
        except Exception:
            continue
        base_txt = "\n".join(git_lines(["show", f"{change_base}:{rel}"]))
        for node in ast.walk(st_tree):
            if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Name):
                continue
            fname = node.func.id
            if fname == "exports_files" and not rel.startswith(exempt_prefixes):
                exported = []
                if node.args and isinstance(node.args[0], ast.List):
                    for elt in node.args[0].elts:
                        if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                            if elt.value.endswith(".shard.cml") and f'"{elt.value}"' not in base_txt:
                                exported.append(elt.value)
                if exported:
                    _emit(findings, "prefer_cml_shard_over_exports_files", "error", rel, node.lineno, f"'{rel}' exports {', '.join(exported)} via `exports_files(...)` "
                            "instead of defining `cml_shard(...)` target(s).", "Load `cml_shard` from `//build/bazel/rules/components:cml_shard.bzl` "
                            "and declare `cml_shard(name = \"<stem>.shard\", shard = "
                            "\"<stem>.shard.cml\", includes = [...])` instead of `exports_files`.")
            elif fname == "fx_component_manifest":
                by_pkg = {}
                for kw in node.keywords:
                    if kw.arg == "includes" and isinstance(kw.value, ast.List):
                        for elt in kw.value.elts:
                            if isinstance(elt, ast.Constant) and isinstance(elt.value, str):
                                m_sh = re.match(r"^//([^:]+):([^:]+\.shard\.cml)$", elt.value)
                                if m_sh and not m_sh.group(1).startswith(exempt_prefixes):
                                    by_pkg.setdefault(m_sh.group(1), []).append(elt.value)
                for spkg, slabels in sorted(by_pkg.items()):
                    # Pre-existing raw shard includes (all already in the file at the change
                    # base) belong to code owners; only flag includes the change adds.
                    if all(f'"{lbl}"' in base_txt for lbl in slabels):
                        continue
                    if len(slabels) >= 2:
                        _emit(findings, "unrolled_transitive_cml_shard_includes", "error", rel, node.lineno, f"`fx_component_manifest` in '{rel}' lists multiple raw "
                                f"`.shard.cml` files from `//{spkg}` ({', '.join(slabels)}) "
                                "instead of a `cml_shard` target with `includes`.", f"Define `cml_shard` target(s) in `{spkg}/BUILD.bazel` bundling "
                                "its transitive shard `includes` and reference the single "
                                "`cml_shard` label in `fx_component_manifest(includes = [...])`.")


# Newly added Starlark files need the standard Fuchsia copyright header with
# 2026 or the current year. diffengine owns BUILD.bazel in the migrated target
# directories (copyright_header there), so skip those.
def check_new_starlark_copyright_headers(starlark_files):
    import datetime

    cur_year = str(datetime.datetime.now(datetime.timezone.utc).year)
    ok_years = {"2026", cur_year}
    copyright_re = re.compile(r"^#\s*Copyright\s+(\d{4})\s+The\s+Fuchsia\s+Authors\b", re.M)
    diffengine_owned = {f"{td}/BUILD.bazel" for td in target_dirs}
    rels = set(starlark_files)
    for p in changed:
        norm = p.strip("/")
        if norm.startswith("third_party/") and not norm.startswith(RUST_CRATES + "compat/"):
            continue
        if os.path.basename(norm) in ("BUILD.bazel", "BUILD") and os.path.isfile(os.path.join(workdir, norm)):
            rels.add(norm)
    for rel in sorted(rels - diffengine_owned):
        if git_lines(["show", f"{change_base}:{rel}"]):
            continue  # Pre-existing files keep their original year.
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                m = copyright_re.search(f.read(512))
        except OSError:
            continue
        header = f"# Copyright {cur_year} The Fuchsia Authors. All rights reserved."
        if not m:
            _emit(findings, "copyright_header", "error", rel, 1,
                  f"Newly added file '{rel}' is missing the standard Fuchsia copyright header with year {cur_year}.",
                  f"Add `{header}` and the 2-line BSD license header at the top of '{rel}'.")
        elif m.group(1) not in ok_years:
            _emit(findings, "copyright_header", "error", rel, 1,
                  f"'{rel}' copyright header uses year {m.group(1)} instead of {cur_year}",
                  f"Update line 1 of newly added file '{rel}' to `{header}`.")


check_generated_vendor_build_hygiene()
check_cml_shard_targets(starlark_rels)
check_new_starlark_copyright_headers(starlark_rels)


# Resolve (target_name, gn_output_name) for a compiled_action(tool = ...) label.
def _resolve_compiled_action_tool_output_name(cur_pkg, tool_lbl):
    if tool_lbl.startswith("@"):
        return "", ""
    if tool_lbl.startswith("//"):
        pkg_part, has_c, t_part = tool_lbl[2:].partition(":")
        t_pkg = pkg_part.strip("/")
        t_name = t_part if has_c else os.path.basename(t_pkg)
    else:
        t_pkg = cur_pkg
        t_name = tool_lbl.lstrip(":")
    if not t_name:
        return "", ""
    cand_bz = os.path.join(workdir, t_pkg, "BUILD.bazel") if t_pkg else ""
    if not cand_bz or not os.path.isfile(cand_bz):
        return t_name, t_name
    try:
        with open(cand_bz, "r", encoding="utf-8") as f:
            bz_src = f.read()
        bz_tree = ast.parse(bz_src, filename=cand_bz)
    except Exception:
        return t_name, t_name
    for node in ast.walk(bz_tree):
        if not (isinstance(node, ast.Call) and isinstance(node.func, ast.Name)):
            continue
        r_id = node.func.id
        if r_id not in (
            "rustc_binary",
            "rust_binary",
            "fx_cc_binary",
            "cc_binary",
            "go_binary",
            "py_binary",
        ):
            continue
        k_strs = {
            kw.arg: kw.value.value
            for kw in node.keywords
            if kw.arg
            and isinstance(kw.value, ast.Constant)
            and isinstance(kw.value.value, str)
        }
        if k_strs.get("name") != t_name:
            continue
        if r_id in ("rustc_binary", "rust_binary"):
            gn_out = (
                k_strs.get("binary_name")
                or k_strs.get("crate_name")
                or t_name.replace("-", "_")
            )
        else:
            gn_out = k_strs.get("output_name") or k_strs.get("binary_name") or t_name
        return t_name, gn_out
    return t_name, t_name


# Audit compiled_action() target invocations in a BUILD / BUILD.bazel file.
def _check_compiled_action_build_targets(rel, st_src, st_tree, loc_re):
    base_flat = re.sub(
        r"\s+", "", "\n".join(git_lines(["show", f"{change_base}:{rel}"]))
    )
    st_lines = st_src.splitlines()
    cur_pkg = os.path.dirname(rel).strip("/")
    for node in ast.walk(st_tree):
        if not (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "compiled_action"
        ):
            continue
        seg = ast.get_source_segment(st_src, node) or ""
        if seg and base_flat and re.sub(r"\s+", "", seg) in base_flat:
            continue
        ca_name = ""
        depfile_val = ""
        tool_val = ""
        tool_out_val = ""
        tool_out_line = getattr(node, "lineno", 1)
        unannotated_deps = set()
        all_deps = set()
        args_nodes = []
        for kw in node.keywords:
            if kw.arg == "name" and isinstance(kw.value, ast.Constant):
                ca_name = str(kw.value.value)
            elif kw.arg == "depfile" and isinstance(kw.value, ast.Constant):
                depfile_val = str(kw.value.value)
            elif kw.arg == "tool" and isinstance(kw.value, ast.Constant):
                tool_val = str(kw.value.value)
            elif kw.arg == "tool_output_name" and isinstance(kw.value, ast.Constant):
                tool_out_val = str(kw.value.value)
                tool_out_line = kw.lineno
            elif kw.arg == "deps":
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        dlbl = sub.value
                        dline = (
                            st_lines[sub.lineno - 1]
                            if 1 <= getattr(sub, "lineno", 0) <= len(st_lines)
                            else ""
                        )
                        forms = {dlbl}
                        if not dlbl.startswith(("//", "@")):
                            bare = dlbl.lstrip(":")
                            forms.update({bare, ":" + bare})
                        all_deps.update(forms)
                        if "@bazel2gn:path_overwrite:" not in dline:
                            unannotated_deps.update(forms)
            elif kw.arg == "args":
                for sub in ast.walk(kw.value):
                    if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                        args_nodes.append(sub)
        depfile_forms = (
            {depfile_val, depfile_val.lstrip(":"), ":" + depfile_val.lstrip(":")}
            if depfile_val
            else set()
        )
        for a_node in args_nodes:
            aline = (
                st_lines[a_node.lineno - 1]
                if 1 <= getattr(a_node, "lineno", 0) <= len(st_lines)
                else ""
            )
            if "@bazel2gn:raw_overwrite:" in aline:
                continue
            for m in loc_re.finditer(a_node.value):
                ref_lbl = m.group(1)
                if depfile_forms and ref_lbl in depfile_forms:
                    clean_df = depfile_val.lstrip(":")
                    msg = (
                        f"Target '{ca_name}' (compiled_action) references depfile "
                        f"'{depfile_val}' via `{m.group(0)}` in `args`. `depfile` is "
                        "a string attribute unused by Bazel and forwarded to GN by "
                        "bazel2gn, so `$(location ...)` / `$(execpath ...)` cannot "
                        "expand it in Bazel."
                    )
                    rem = (
                        f"Replace `{m.group(0)}` with `$(RULEDIR)/{clean_df}` in "
                        f"`args` of `{ca_name}` in '{rel}' (`bazel2gn` translates "
                        "`$(RULEDIR)` to `rebase_path(target_gen_dir, root_build_dir)`)."
                    )
                    _emit(
                        findings,
                        "compiled_action_depfile_location_substitution",
                        "error",
                        rel,
                        getattr(a_node, "lineno", node.lineno),
                        msg,
                        rem,
                    )
                elif ref_lbl in unannotated_deps or ref_lbl in all_deps:
                    msg = (
                        f"Target '{ca_name}' (compiled_action) references `deps` "
                        f"entry '{ref_lbl}' via `{m.group(0)}` in `args`. "
                        "`compiled_action` only expands `$(location ...)` for `srcs` "
                        "and `outs`, and `bazel2gn` rejects un-annotated `deps` "
                        "labels in `args`."
                    )
                    rem = (
                        f"Move '{ref_lbl}' from `deps` to `srcs` in `{ca_name}` in "
                        f"'{rel}' (adding `# @bazel2gn:path_overwrite:<gn_path>` if "
                        "its GN path differs), or avoid `$(location ...)` on `deps`."
                    )
                    _emit(
                        findings,
                        "compiled_action_deps_location_substitution",
                        "error",
                        rel,
                        getattr(a_node, "lineno", node.lineno),
                        msg,
                        rem,
                    )
        if tool_val:
            t_name, gn_out = _resolve_compiled_action_tool_output_name(cur_pkg, tool_val)
            if t_name and gn_out and gn_out != t_name and not tool_out_val:
                msg = (
                    f"Target '{ca_name}' (compiled_action) uses `tool = "
                    f"\"{tool_val}\"` whose GN output filename is `{gn_out}` "
                    f"(different from target name `{t_name}`), but omits "
                    f"`tool_output_name = \"{gn_out}\"` (`bazel2gn` does not "
                    "auto-infer `tool_output_name`)."
                )
                rem = (
                    f"Add `tool_output_name = \"{gn_out}\"` to `{ca_name}` in "
                    f"'{rel}' and re-run `fx bazel2gn -d {cur_pkg}`."
                )
                _emit(
                    findings,
                    "compiled_action_missing_tool_output_name",
                    "error",
                    rel,
                    getattr(node, "lineno", 1),
                    msg,
                    rem,
                )
            elif t_name and tool_out_val and tool_out_val == t_name:
                msg = (
                    f"Target '{ca_name}' (compiled_action) sets "
                    f"`tool_output_name = \"{tool_out_val}\"` matching the target "
                    f"name of `tool = \"{tool_val}\"`, which is already the default."
                )
                rem = (
                    f"Remove `tool_output_name = \"{tool_out_val}\"` from "
                    f"`{ca_name}` in '{rel}'."
                )
                _emit(
                    findings,
                    "redundant_default_attribute",
                    "error",
                    rel,
                    tool_out_line,
                    msg,
                    rem,
                )


# Audit Starlark action rule definitions under build/bazel/rules/ when modified.
def _check_starlark_rule_action_hygiene(rel, bzl_txt):
    # Reject declaring a GN-only bazel2gn-forwarded attribute as attr.output in any .bzl rule.
    gn_only_out_re = re.compile(
        r'"([a-z_][a-z0-9_]*)"\s*:\s*attr\.output(?:_list)?\([^)]*'
        r"unused by Bazel[^)]*forwarded to GN",
        re.S | re.I,
    )
    for m in gn_only_out_re.finditer(bzl_txt):
        attr_name = m.group(1)
        lineno = bzl_txt.count("\n", 0, m.start()) + 1
        msg = (
            f"'{rel}' declares GN-only forwarded attribute `{attr_name}` as "
            "`attr.output` / `attr.output_list`, forcing Bazel actions to write a file "
            "that Bazel ignores."
        )
        rem = (
            f"Declare `{attr_name}` as `attr.string` in '{rel}', document it as unused "
            "by Bazel and forwarded to GN by `bazel2gn`, and omit it from Bazel action "
            "`outputs`."
        )
        _emit(findings, "gn_only_attr_declared_as_bazel_output", "error", rel, lineno, msg, rem)

    if rel != "build/bazel/rules/compiled_action.bzl":
        return
    if (
        "ctx.attr.deps" in bzl_txt.split("ctx.actions.run")[0]
        or re.search(r"`srcs`,\s*`deps`", bzl_txt)
        or re.search(
            r'"deps"\s*:\s*attr\.label_list\([^)]*\$\(location', bzl_txt, re.S
        )
    ):
        msg = (
            f"'{rel}' includes `deps` in `ctx.expand_location` targets or documents "
            "`deps` as supported in `$(location ...)`. `deps` are plain action inputs "
            "and `bazel2gn` cannot translate an un-annotated `deps` label in `args` to "
            "its GN output path."
        )
        rem = (
            f"In '{rel}', pass `targets = ctx.attr.srcs` directly to "
            "`ctx.expand_location` (without `ctx.attr.deps`), and remove `deps` from "
            "the `$(location ...)` documentation at the top of the file and in the "
            "`deps` docstring."
        )
        _emit(
            findings,
            "compiled_action_bzl_deps_in_expand_location",
            "error",
            rel,
            54,
            msg,
            rem,
        )
    if (
        re.search(r'"depfile"\s*:\s*attr\.output\b', bzl_txt)
        or "ctx.outputs.depfile" in bzl_txt
        or re.search(r"`outs`,\s*or\s*`depfile`", bzl_txt)
    ):
        msg = (
            f"'{rel}' declares `depfile` as `attr.output` / action output or lists "
            "`depfile` under `$(location ...)`. Fuchsia's Bazel build does not consume "
            "depfiles, so declaring `depfile` as a Bazel output requires tools to write "
            "a file Bazel ignores."
        )
        rem = (
            f"In '{rel}', change `depfile` to `attr.string` documented as unused by "
            "Bazel and forwarded to GN `compiled_action()` by `bazel2gn`, remove "
            "`ctx.outputs.depfile` from `_compiled_action_impl` so "
            "`outputs = ctx.outputs.outs`, and drop `depfile` from the "
            "`$(location ...)` list in the module docstring (pass `$(RULEDIR)/<depfile>` "
            "in `args` instead)."
        )
        _emit(
            findings,
            "compiled_action_bzl_depfile_output",
            "error",
            rel,
            126,
            msg,
            rem,
        )


# Audit build/tools/bazel2gn/compiled_action.go and companion docs when modified.
def _check_bazel2gn_compiled_action_converter():
    ca_go = "build/tools/bazel2gn/compiled_action.go"
    if ca_go not in changed or not os.path.isfile(os.path.join(workdir, ca_go)):
        return
    go_txt = open(os.path.join(workdir, ca_go), encoding="utf-8").read()
    b2gn_go_path = os.path.join(workdir, "build/tools/bazel2gn/bazel2gn.go")
    b2gn_txt = (
        open(b2gn_go_path, encoding="utf-8").read()
        if os.path.isfile(b2gn_go_path)
        else ""
    )
    doc_txts = []
    for doc_rel in ("build/tools/bazel2gn/README.md", "build/tools/bazel2gn/AGENTS.md"):
        doc_path = os.path.join(workdir, doc_rel)
        if os.path.isfile(doc_path):
            doc_txts.append(open(doc_path, encoding="utf-8").read())
    combined_docs = "\n".join(doc_txts)
    claims_auto_infer = False
    for line in combined_docs.splitlines():
        if "tool_output_name" in line and re.search(r"\binfer\w*\b", line, re.I):
            if not re.search(r"\b(?:not|never|no|without)\b[^.\n]*\binfer", line, re.I):
                claims_auto_infer = True
                break
    if (
        "resolveCompiledActionTargets" in go_txt
        or "hasRawToolOutputNameAttr" in go_txt
        or "rawToolOutputNameRE" in go_txt
        or "resolveCompiledActionTargets" in b2gn_txt
        or claims_auto_infer
    ):
        msg = (
            f"'{ca_go}' (or companion bazel2gn files) implements or documents "
            "same-file `tool_output_name` auto-inference, adding a whole-file rewrite "
            "pass when `tool_output_name` should be specified explicitly."
        )
        rem = (
            f"Remove same-file `tool_output_name` inference passes from '{ca_go}' and "
            "'build/tools/bazel2gn/bazel2gn.go', and rely solely on explicit "
            "`tool_output_name` attributes in `compiled_action()`."
        )
        _emit(
            findings,
            "bazel2gn_compiled_action_tool_output_name_inference",
            "error",
            ca_go,
            97,
            msg,
            rem,
        )
    if (
        not re.search(r'case\s+"deps"|case\s+"srcs",\s*"deps"', go_txt)
        or "path_overwrite" not in go_txt
    ):
        msg = (
            f"'{ca_go}' does not inspect `deps` in `compiledActionPaths` to reject "
            "`$(location <label>)` / `$(execpath <label>)` on un-annotated `deps` "
            "entries in `compiledActionArgToGN`."
        )
        rem = (
            f"In `compiledActionPaths` in '{ca_go}', collect `deps` labels (both `:x` "
            "and `x` forms) that lack `@bazel2gn:path_overwrite`, map path-overwritten "
            "`deps` entries the same way as path-overwritten `srcs`, and in "
            "`compiledActionArgToGN` return an error when an arg's label refers to an "
            "un-annotated `deps` entry. Update doc comments in `compiled_action.go`, "
            "`compiled_action.bzl`, and `README.md` to match."
        )
        _emit(
            findings,
            "bazel2gn_compiled_action_unannotated_deps_location",
            "error",
            ca_go,
            392,
            msg,
            rem,
        )


# Check compiled_action() target hygiene in BUILD.bazel files and rule/converter invariants
# when build/bazel/rules/*.bzl or build/tools/bazel2gn/* is modified.
def check_compiled_action_hygiene(starlark_files):
    loc_re = re.compile(r"\$\((?:location|execpath)\s+([^()\s]+)\s*\)")

    # 1. Check compiled_action() target invocations in BUILD.bazel files.
    for rel in sorted(starlark_files):
        if not os.path.basename(rel).startswith("BUILD"):
            continue
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                st_src = f.read()
            st_tree = ast.parse(st_src, filename=rel)
        except Exception:
            continue
        _check_compiled_action_build_targets(rel, st_src, st_tree, loc_re)

    # 2. Check modified Starlark rule files under build/bazel/rules/.
    for ch_rel in sorted(changed):
        if (
            ch_rel.startswith("build/bazel/rules/")
            and ch_rel.endswith(".bzl")
            and os.path.isfile(os.path.join(workdir, ch_rel))
        ):
            bzl_txt = open(os.path.join(workdir, ch_rel), encoding="utf-8").read()
            _check_starlark_rule_action_hygiene(ch_rel, bzl_txt)

    # 3. Check build/tools/bazel2gn/ compiled_action converter invariants when modified.
    _check_bazel2gn_compiled_action_converter()


check_compiled_action_hygiene(starlark_rels)


# Audit a rust_shared_library (`cdylib`) wrapper macro in build/bazel/rules/rust/
# for DepInfo forwarding, distinct direct_cc_infos vs cc_infos merging, public_deps
# on the generated headers library, and output_name reasoning comments.
def _check_rust_shared_lib_hdrs_wrapper(rel, bzl_txt):
    if "rust_shared_library" not in bzl_txt:
        return
    if "cc_common.merge_cc_infos" in bzl_txt:
        if "cdylib[rust_common.dep_info]" not in bzl_txt:
            msg = (
                f"'{rel}' wraps `rust_shared_library` with `hdrs` without forwarding "
                "`cdylib[rust_common.dep_info]`. Without `DepInfo` on the wrapper rule, "
                "`fx_packaged_binary` cannot collect transitive runtime shared libraries "
                "from `DepInfo.transitive_noncrates` when `hdrs` is set."
            )
            rem = (
                f"In '{rel}', add `cdylib[rust_common.dep_info]` to the providers "
                "returned by `_cdylib_with_hdrs_impl` and list `rust_common.dep_info` "
                "in `shared_library` `providers` and `_cdylib_with_hdrs` `provides`."
            )
            _emit(
                findings,
                "rust_shared_lib_wrapper_missing_dep_info",
                "error",
                rel,
                38,
                msg,
                rem,
            )
        m_merge = re.search(
            r"cc_common\.merge_cc_infos\(\s*direct_cc_infos\s*=\s*\[([^\]]*)\]\s*,"
            r"\s*cc_infos\s*=\s*\[([^\]]*)\]",
            bzl_txt,
            re.S,
        )
        if m_merge:
            d_arg = re.sub(r"\s+", "", m_merge.group(1))
            c_arg = re.sub(r"\s+", "", m_merge.group(2))
            prefix_Window = bzl_txt[max(0, m_merge.start() - 260) : m_merge.start()]
            has_merge_comment = "#" in prefix_Window and "direct_cc_infos" in prefix_Window
            if d_arg == c_arg or "cdylib[CcInfo]" in d_arg or not has_merge_comment:
                msg = (
                    f"'{rel}' sets `direct_cc_infos` and `cc_infos` to the same value "
                    "or lacks a comment explaining why `headers` goes in "
                    "`direct_cc_infos` and `cdylib` goes in `cc_infos`."
                )
                rem = (
                    f"In '{rel}', pass `direct_cc_infos = [ctx.attr.headers[CcInfo]]` "
                    "and `cc_infos = [cdylib[CcInfo]]` to `cc_common.merge_cc_infos` "
                    "with a comment explaining that `direct_cc_infos` re-exports public "
                    "headers as direct headers while `cc_infos` merges the cdylib's "
                    "linking context."
                )
                _emit(
                    findings,
                    "rust_shared_lib_wrapper_merge_cc_infos",
                    "error",
                    rel,
                    33,
                    msg,
                    rem,
                )
    m_hdr_lib = re.search(r"fx_cc_library\(\s*name\s*=\s*headers_target_name\b.*?\)", bzl_txt, re.S)
    if m_hdr_lib and not re.search(r"\bdeps\s*=\s*public_deps\b", m_hdr_lib.group(0)):
        msg = (
            f"'{rel}' generates `fx_cc_library(name = headers_target_name, hdrs = hdrs)` "
            "without `deps = public_deps`, so C/C++ dependents in Bazel do not receive "
            "header compilation contexts from `public_deps` as they do in GN."
        )
        rem = (
            f"In '{rel}', pass `deps = public_deps` to "
            "`fx_cc_library(name = headers_target_name, ...)` and document in the "
            "`public_deps` attribute docstring that `public_deps` are also added to "
            "`deps` of the generated headers library when `hdrs` is set."
        )
        _emit(
            findings,
            "rust_shared_lib_headers_missing_public_deps",
            "error",
            rel,
            140,
            msg,
            rem,
        )
    if "Keep Bazel and GN rustc_cdylib() output filenames" in bzl_txt:
        msg = (
            f"'{rel}' comment above `if output_name:` restates behavior instead of "
            "explaining why Bazel and GN `cdylib` output filenames (`lib<output_name>.so`) "
            "must match."
        )
        rem = (
            f"In '{rel}', update the comment above `if output_name:` to explain why "
            "Bazel and GN must produce the same `lib<output_name>.so` filename (because "
            "`.cml` `program.binary`, `dlopen()` callers, and `bazel2gn` component "
            "unwrapping are shared between both build systems)."
        )
        _emit(
            findings,
            "rust_shared_lib_output_name_comment_reasoning",
            "error",
            rel,
            84,
            msg,
            rem,
        )


# Audit modified .bzl files for custom C/C++ shared-library wrappers that should
# use fx_cc_shared_library, and fx_packaged_binary rejecting CcSharedLibraryInfo.
def _check_custom_cc_shared_library_macros():
    for ch_rel in sorted(changed):
        norm_rel = ch_rel.strip("/")
        if not norm_rel.endswith(".bzl"):
            continue
        abs_p = os.path.join(workdir, norm_rel)
        if not os.path.isfile(abs_p):
            continue
        bzl_txt = open(abs_p, encoding="utf-8").read()
        if "_fx_packaged_binary_impl" in bzl_txt and "CcSharedLibraryInfo" in bzl_txt:
            msg = (
                f"'{norm_rel}' rejects `CcSharedLibraryInfo` targets with `fail()`, "
                "preventing `fx_packaged_binary` from packaging `fx_cc_shared_library` "
                "targets (whose `.so` is `DefaultInfo.files[0]`)."
            )
            rem = (
                f"In '{norm_rel}', remove the `CcSharedLibraryInfo` `fail()` check in "
                "`_fx_packaged_binary_impl` and remove the unused `CcSharedLibraryInfo` "
                "load."
            )
            _emit(
                findings,
                "packaged_binary_rejects_cc_shared_library",
                "error",
                norm_rel,
                87,
                msg,
                rem,
            )
        if not norm_rel.startswith("build/bazel/rules/cc/") and (
            re.search(r"\blinkshared\s*=\s*True\b", bzl_txt)
            or "create_library_to_link" in bzl_txt
        ):
            msg = (
                f"'{norm_rel}' builds a C/C++ shared library using `fx_cc_binary("
                "linkshared = True)` / `cc_common.create_library_to_link` and a custom "
                "`rule()` wrapper instead of `fx_cc_shared_library()`."
            )
            rem = (
                f"In '{norm_rel}', replace the `fx_cc_binary(linkshared = True)` and "
                "custom wrapper rule with `fx_cc_library(name = name + \"_impl\", srcs = "
                "srcs, deps = all_deps, alwayslink = True, ...)` plus "
                "`fx_cc_shared_library(name = name, shared_lib_name = "
                "\"lib{}.so\".format(out_name), user_link_flags = linkopts, deps = "
                "[\":\" + name + \"_impl\"], ...)`, drop unused `hdrs`/`public_deps` "
                "parameters, and drop the `CcSharedLibraryInfo` `fail()` in "
                "`build/bazel/rules/packages/fx_packaged_binary.bzl`."
            )
            _emit(
                findings,
                "custom_cc_shared_library_wrapper",
                "error",
                norm_rel,
                100,
                msg,
                rem,
            )


# Audit a Fuchsia rustc_*.bzl wrapper macro file for custom wrapper rules,
# deprecated with_*_unit_tests attributes, and unprefixed bazel2gn attributes.
def _check_rustc_macro_bzl(rel, bzl_txt):
    if (
        "_rustc_staticlib_cc_wrapper" in bzl_txt
        or "create_crate_info" in bzl_txt
        or re.search(r'"non_test_deps"\s*:\s*attr\.', bzl_txt)
        or (
            "rust_static_library" in bzl_txt
            and re.search(r'"hdrs"\s*:\s*attr\.', bzl_txt)
        )
    ):
        msg = (
            f"'{rel}' defines a custom `rule()` wrapper (`_rustc_staticlib_cc_wrapper`) "
            "or exposes `hdrs` / `non_test_deps` / archive-renaming `output_name`. "
            "`rustc_staticlib()` must be a thin `rust_static_library` macro like "
            "`rustc_library()`: GN never writes `lib<output_name>.a` (`output_name` is "
            "a GN-only attribute ignored in Bazel), C/C++ headers use "
            "`cc_library(hdrs = [...], deps = [\":<staticlib>\"])`, and tests that "
            "exclude a library dependency use a standalone `rustc_test()` with own "
            "`srcs`/`crate_root`/`deps`."
        )
        rem = (
            f"In '{rel}', drop `_rustc_staticlib_cc_wrapper_impl` and "
            "`_rustc_staticlib_cc_wrapper` (and unused `@bazel_tools//tools/cpp:...`, "
            "`@rules_cc//...`, and `rust_common` loads), remove `hdrs` and "
            "`non_test_deps`, accept `output_name` as a `bazel2gn`-only attribute "
            "ignored in Bazel (`output_name,  # buildifier: disable=unused-variable`), "
            "and call `rust_static_library(name = name, ...)` directly."
        )
        _emit(findings, "rustc_macro_custom_wrapper_rule", "error", rel, 28, msg, rem)

    _check_rust_shared_lib_hdrs_wrapper(rel, bzl_txt)

    if (
        "generate_unit_tests" in bzl_txt
        or re.search(
            r'"(?:with_unit_tests|with_host_unit_tests|test_deps)"\s*:', bzl_txt
        )
    ):
        msg = (
            f"'{rel}' supports deprecated `with_unit_tests` / `with_host_unit_tests` "
            "(`test_deps`, `generate_unit_tests`). New `rustc_*` macros must not "
            "support `with_*_unit_tests`; unit tests must use standalone `rustc_test()` "
            "or `host_rustc_test()` targets."
        )
        rem = (
            f"In '{rel}', remove the `generate_unit_tests` load and call, "
            "`test_lint_config`, and the `with_host_unit_tests`, `with_unit_tests`, "
            "`test_deps`, and `non_test_deps` parameters/attributes and docstrings."
        )
        _emit(
            findings,
            "rustc_macro_deprecated_with_unit_tests",
            "error",
            rel,
            180,
            msg,
            rem,
        )

    if re.search(r'"(?:quiet_clippy|disable_rustdoc)"\s*:\s*attr\.', bzl_txt):
        msg = (
            f"'{rel}' declares unprefixed `quiet_clippy` / `disable_rustdoc` "
            "attributes. Attributes on `rustc_*` macros that exist only for `bazel2gn` "
            "passthrough to GN must be prefixed with `gn_` (`gn_quiet_clippy` and "
            "`gn_disable_rustdoc`)."
        )
        rem = (
            f"In '{rel}', rename `quiet_clippy` and `disable_rustdoc` to "
            "`gn_quiet_clippy` and `gn_disable_rustdoc` (both ignored in Bazel with "
            "`# buildifier: disable=unused-variable`), and map "
            "`\"gn_disable_rustdoc\": \"disable_rustdoc\"` and "
            "`\"gn_quiet_clippy\": \"quiet_clippy\"` in `rustCommonAttrMap` in "
            "'build/tools/bazel2gn/constants.go'."
        )
        _emit(
            findings,
            "rustc_macro_unprefixed_gn_passthrough_attrs",
            "error",
            rel,
            190,
            msg,
            rem,
        )


# Audit companion clippy_test.bzl and rustc_staticlib.gni files when modified.
def _check_rustc_macro_tests_and_gni():
    ct_bzl = "build/bazel/rules/rust/tests/clippy_test.bzl"
    if ct_bzl in changed and os.path.isfile(os.path.join(workdir, ct_bzl)):
        ct_txt = open(os.path.join(workdir, ct_bzl), encoding="utf-8").read()
        if (
            re.search(r"rustc_staticlib\([^)]*with_unit_tests", ct_txt, re.S)
            or "staticlib_default_test" in ct_txt
            or "staticlib_disabled_test" in ct_txt
        ):
            msg = (
                f"'{ct_bzl}' passes `with_unit_tests = True` to `rustc_staticlib` or "
                "references `staticlib_*_test` targets, which `rustc_staticlib` must "
                "not generate."
            )
            rem = (
                f"In '{ct_bzl}', remove `with_unit_tests = True` from "
                "`staticlib_default` and `staticlib_disabled` and remove the "
                "`staticlib_default_unit_test` and `staticlib_disabled_unit_test` "
                "entries from `clippy_test_suite`."
            )
            _emit(
                findings,
                "rustc_macro_deprecated_with_unit_tests",
                "error",
                ct_bzl,
                139,
                msg,
                rem,
            )

    gni_rel = "build/rust/rustc_staticlib.gni"
    if gni_rel in changed and os.path.isfile(os.path.join(workdir, gni_rel)):
        gni_txt = open(os.path.join(workdir, gni_rel), encoding="utf-8").read()
        expected_assert = 'assert(_with_unit_tests, "use deps if not generating unit tests")'
        if expected_assert not in gni_txt:
            msg = (
                f"'{gni_rel}' removes `{expected_assert}` on `non_test_deps`. In "
                "Bazel, a standalone `rustc_test()` with its own `srcs`/`crate_root`/"
                "`deps` excludes non-test dependencies without filtering, so the GN "
                "side uses plain `deps` when `with_unit_tests` is not set."
            )
            rem = (
                f"In '{gni_rel}', restore `{expected_assert}` and the original "
                "`non_test_deps` doc comment."
            )
            _emit(
                findings,
                "gn_staticlib_non_test_deps_assert_removed",
                "error",
                gni_rel,
                298,
                msg,
                rem,
            )


# Audit bazel2gn converter and tests for rustc_staticlib attribute hygiene.
def _check_bazel2gn_rust_staticlib_hygiene():
    if not any(p.strip("/").startswith("build/tools/bazel2gn/") for p in changed):
        return
    const_go = "build/tools/bazel2gn/constants.go"
    if const_go in changed and os.path.isfile(os.path.join(workdir, const_go)):
        c_txt = open(os.path.join(workdir, const_go), encoding="utf-8").read()
        if "rustc_staticlib" in c_txt and (
            '"gn_quiet_clippy"' not in c_txt or '"gn_disable_rustdoc"' not in c_txt
        ):
            msg = (
                f"'{const_go}' does not map `gn_quiet_clippy` to `quiet_clippy` and "
                "`gn_disable_rustdoc` to `disable_rustdoc` in `rustCommonAttrMap`."
            )
            rem = (
                f"In `rustCommonAttrMap` in '{const_go}', add "
                "`\"gn_disable_rustdoc\": \"disable_rustdoc\"` and "
                "`\"gn_quiet_clippy\": \"quiet_clippy\"`."
            )
            _emit(
                findings,
                "bazel2gn_missing_gn_prefixed_rust_attrs",
                "error",
                const_go,
                328,
                msg,
                rem,
            )

    b2gn_go = "build/tools/bazel2gn/bazel2gn.go"
    if b2gn_go in changed and os.path.isfile(os.path.join(workdir, b2gn_go)):
        b_txt = open(os.path.join(workdir, b2gn_go), encoding="utf-8").read()
        if '"non_test_deps"' in b_txt or re.search(
            r'case\s+"api",\s*"outputs",\s*"public"', b_txt
        ):
            msg = (
                f"'{b2gn_go}' adds `\"non_test_deps\"` or `\"public\"` to "
                "`attrAssignmentToGN`, which are not used by `rustc_staticlib` "
                "(and adding `\"public\"` alters `hdrs` path-overwrite behavior)."
            )
            rem = (
                f"In '{b2gn_go}', remove `\"non_test_deps\"` and `\"public\"` from "
                "`attrAssignmentToGN`, and revert "
                "the change to `TestPathOverwriteAnnotations` in "
                "'build/tools/bazel2gn/bazel2gn_test.go'."
            )
            _emit(
                findings,
                "bazel2gn_unneeded_staticlib_attr_transformers",
                "error",
                b2gn_go,
                831,
                msg,
                rem,
            )

    b2gn_test = "build/tools/bazel2gn/bazel2gn_test.go"
    if b2gn_test in changed and os.path.isfile(os.path.join(workdir, b2gn_test)):
        bt_txt = open(os.path.join(workdir, b2gn_test), encoding="utf-8").read()
        m_st = re.search(
            r'name:\s*"rustc_staticlib".*?wantGN:\s*`.*?`\s*,', bt_txt, re.S
        )
        st_blk = m_st.group(0) if m_st else ""
        if (
            any(
                k in st_blk
                for k in (
                    "hdrs =",
                    "non_test_deps =",
                    "test_deps =",
                    "with_unit_tests =",
                    "with_host_unit_tests =",
                )
            )
            or re.search(
                r"(?<![_\w])(?:quiet_clippy|disable_rustdoc)\s*=\s*True", st_blk
            )
            or "should_be_ignored.h" not in bt_txt
        ):
            msg = (
                f"'{b2gn_test}' tests removed attributes (`hdrs`, `non_test_deps`, "
                "`test_deps`, `with_unit_tests`, `with_host_unit_tests`) or unprefixed "
                "`quiet_clippy` / `disable_rustdoc` on `rustc_staticlib` / "
                "`rust_static_library`, or modifies `TestPathOverwriteAnnotations`."
            )
            rem = (
                f"In '{b2gn_test}', update the `rustc_staticlib` test case to use "
                "`gn_disable_rustdoc = True` and `gn_quiet_clippy = True`, remove "
                "`hdrs`, `non_test_deps`, `test_deps`, `with_unit_tests`, and "
                "`with_host_unit_tests` (and their `wantGN` lines), and restore "
                "`should_be_ignored.h` in `TestPathOverwriteAnnotations`."
            )
            _emit(
                findings,
                "bazel2gn_test_deprecated_staticlib_attrs",
                "error",
                b2gn_test,
                392,
                msg,
                rem,
            )

    rtc_test = "build/tools/bazel2gn/rust_test_crate_test.go"
    if rtc_test in changed and os.path.isfile(os.path.join(workdir, rtc_test)):
        rt_txt = open(os.path.join(workdir, rtc_test), encoding="utf-8").read()
        if "non_test_deps" in rt_txt:
            msg = (
                f"'{rtc_test}' passes `non_test_deps` on `rustc_staticlib` in "
                "`TestRustTestStaticlibCrate` (`rustc_staticlib` does not have "
                "`non_test_deps`)."
            )
            rem = (
                "Remove `non_test_deps` from `bazel` and `wantGN` in "
                f"`TestRustTestStaticlibCrate` in '{rtc_test}'."
            )
            _emit(
                findings,
                "bazel2gn_test_deprecated_staticlib_attrs",
                "error",
                rtc_test,
                277,
                msg,
                rem,
            )

    for tag_f in ("build/tools/bazel2gn/tags.go", "build/tools/bazel2gn/tags_test.go"):
        if tag_f in changed and os.path.isfile(os.path.join(workdir, tag_f)):
            t_txt = open(os.path.join(workdir, tag_f), encoding="utf-8").read()
            if "rustc_staticlib" in t_txt:
                msg = (
                    f"'{tag_f}' adds `rustc_staticlib` `with_unit_tests` handling for "
                    "`exclude_toolchain_tags`, but `rustc_staticlib` does not support "
                    "`with_unit_tests`."
                )
                rem = f"Revert '{tag_f}' (`git checkout {change_base} -- {tag_f}`)."
                _emit(
                    findings,
                    "bazel2gn_tags_staticlib_with_unit_tests",
                    "error",
                    tag_f,
                    34,
                    msg,
                    rem,
                )


# Audit bazel2gn_test.go against newly added or modified Starlark macros so test
# cases do not pass deprecated with_unit_tests or removed hdrs/public_deps attrs.
def _check_bazel2gn_test_macro_attr_parity():
    b2gn_test = "build/tools/bazel2gn/bazel2gn_test.go"
    if b2gn_test not in changed or not os.path.isfile(os.path.join(workdir, b2gn_test)):
        return
    bt_txt = open(os.path.join(workdir, b2gn_test), encoding="utf-8").read()
    for ch_rel in sorted(changed):
        norm_rel = ch_rel.strip("/")
        if not norm_rel.endswith(".bzl"):
            continue
        abs_p = os.path.join(workdir, norm_rel)
        if not os.path.isfile(abs_p):
            continue
        bzl_txt = open(abs_p, encoding="utf-8").read()
        for m_macro in re.finditer(
            r"(?m)^(?:def\s+([a-z][a-z0-9_]*)\s*\(|([a-z][a-z0-9_]*)\s*=\s*macro\s*\()",
            bzl_txt,
        ):
            m_name = m_macro.group(1) or m_macro.group(2)
            if not m_name or m_name.startswith("_"):
                continue
            for m_call in re.finditer(
                r"\b" + re.escape(m_name) + r"\(\s*(.*?)\n\)", bt_txt, re.S
            ):
                call_blk = m_call.group(1)
                lineno = bt_txt.count("\n", 0, m_call.start()) + 1
                if (
                    "with_unit_tests" not in bzl_txt
                    and re.search(r"\bwith_(?:host_)?unit_tests\s*=", call_blk)
                ):
                    msg = (
                        f"'{b2gn_test}' passes `with_unit_tests` to `{m_name}()`, "
                        f"which '{norm_rel}' does not support."
                    )
                    rem = (
                        f"In '{b2gn_test}', remove `with_unit_tests` from the "
                        f"`{m_name}` `bazel` input and `wantGN` output."
                    )
                    _emit(
                        findings,
                        "bazel2gn_test_deprecated_macro_attrs",
                        "error",
                        b2gn_test,
                        lineno,
                        msg,
                        rem,
                    )
                if (
                    "fx_cc_shared_library" in bzl_txt
                    and "hdrs" not in bzl_txt
                    and re.search(r"\b(?:hdrs|public_deps)\s*=", call_blk)
                ):
                    msg = (
                        f"'{b2gn_test}' passes removed `hdrs` or `public_deps` "
                        f"attributes to `{m_name}()`, which '{norm_rel}' omits."
                    )
                    rem = (
                        f"In '{b2gn_test}', remove `hdrs`/`public` and `public_deps` "
                        f"from the `{m_name}` `bazel` input and `wantGN` output."
                    )
                    _emit(
                        findings,
                        "bazel2gn_test_deprecated_macro_attrs",
                        "error",
                        b2gn_test,
                        lineno,
                        msg,
                        rem,
                    )


# Audit BootFS package rule (.bzl) definitions for duplicated fx_packaged_binary
# logic, unused binaries/renamed_binaries attributes, and dummy inherit_attrs
# attribute declarations on _build_bootfs_files_package.
def _check_bootfs_package_bzl(rel, bzl_txt):
    if "fx_bootfs_files_package" not in bzl_txt:
        return
    if (
        any(
            sym in bzl_txt
            for sym in (
                "_fx_bootfs_packaged_binaries",
                "_default_bootfs_binary_name",
                "_dedupe_unstripped_binaries",
                "_validate_raw_executable_target",
            )
        )
        or re.search(
            r'"(?:binaries|renamed_binaries|renamed_binary_deps)"\s*:', bzl_txt
        )
        or re.search(r"\b(?:binaries|renamed_binaries)\s*,", bzl_txt)
    ):
        msg = (
            f"'{rel}' re-implements binary packaging (`_fx_bootfs_packaged_binaries`, "
            "`binaries`, `renamed_binaries`, `_default_bootfs_binary_name`, "
            "`_validate_raw_executable_target`, `_dedupe_unstripped_binaries`) "
            "instead of reusing `fx_packaged_binary()` targets passed via `resources`."
        )
        rem = (
            f"In '{rel}', remove `_fx_bootfs_packaged_binaries`, "
            "`_fx_bootfs_packaged_binaries_impl`, `_default_bootfs_binary_name`, "
            "`_dedupe_unstripped_binaries`, and `_validate_raw_executable_target` "
            "(along with their unused loads), and drop `binaries`, "
            "`renamed_binaries`, and `renamed_binary_deps` from the rules, macro, "
            "and docstrings so callers pass `fx_packaged_binary()` targets in "
            "`resources`."
        )
        _emit(
            findings,
            "bootfs_package_duplicates_packaged_binary",
            "error",
            rel,
            119,
            msg,
            rem,
        )

    m_rule = re.search(
        r"_build_bootfs_files_package\s*=\s*rule\((.*?)\n\)", bzl_txt, re.S
    )
    if m_rule and re.search(
        r'"(?:binaries|components)"\s*:\s*attr\.', m_rule.group(1)
    ):
        msg = (
            f"'{rel}' declares `binaries` or `components` on "
            "`_build_bootfs_files_package` only to feed macro docs through "
            "`inherit_attrs` while the macro always passes `[]` to the rule."
        )
        rem = (
            f"In '{rel}', remove `binaries` and `components` from "
            "`_build_bootfs_files_package`'s `attrs` dict (and remove "
            "`binaries = []` from the `_build_bootfs_files_package(...)` call). "
            "Either drop `components` completely (setting `\"components\": None` "
            "in `fx_bootfs_files_package`'s `attrs` and removing `components` from "
            "`_bootfs_rebase_resources`) or declare `\"components\"` directly in "
            "`fx_bootfs_files_package`'s `attrs` dict."
        )
        _emit(
            findings,
            "bootfs_package_rule_dummy_inherited_attrs",
            "error",
            rel,
            354,
            msg,
            rem,
        )


# Audit package rule tests and commit message for removed BootFS binary attributes.
def _check_bootfs_package_tests_and_commit():
    pkg_tests = "build/bazel/rules/packages/tests/BUILD.bazel"
    if pkg_tests in changed and os.path.isfile(os.path.join(workdir, pkg_tests)):
        pt_txt = open(os.path.join(workdir, pkg_tests), encoding="utf-8").read()
        if (
            "example_bootfs_non_executable_binary" in pt_txt
            or "example_bootfs_rust_bin" in pt_txt
            or re.search(
                r"fx_bootfs_files_package\([^)]*\b(?:binaries|renamed_binaries)\s*=",
                pt_txt,
                re.S,
            )
        ):
            msg = (
                f"'{pkg_tests}' still tests removed `binaries` or "
                "`renamed_binaries` attributes (`example_bootfs_rust_bin`, "
                "`example_bootfs_non_executable_binary_failure_test`) on "
                "`fx_bootfs_files_package()`."
            )
            rem = (
                f"In '{pkg_tests}', remove `example_bootfs_non_executable_binary`, "
                "`example_bootfs_non_executable_binary_failure_test` (and its entry "
                "in `quick_build_only_tests`), and `example_bootfs_rust_bin`, and "
                "update `example_bootfs_files_package_full` to pass "
                "`fx_packaged_binary()` targets via `resources` instead of "
                "`binaries` and `renamed_binaries`."
            )
            _emit(
                findings,
                "bootfs_package_test_removed_binary_attrs",
                "error",
                pkg_tests,
                1487,
                msg,
                rem,
            )

    bootfs_bzl = "build/bazel/rules/packages/fx_bootfs_files_package.bzl"
    if bootfs_bzl in changed:
        msg_body = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
        if (
            "renamed_binaries" in msg_body
            or "example_bootfs_non_executable_binary_failure_test" in msg_body
        ):
            msg = (
                "Commit message still references removed `renamed_binaries` or "
                "`example_bootfs_non_executable_binary_failure_test`."
            )
            rem = (
                "Amend the commit message body and `Test:` footer to remove "
                "references to `renamed_binaries` and "
                "`example_bootfs_non_executable_binary_failure_test`."
            )
            _emit(
                findings,
                "commit_body_stale_bootfs_binary_claims",
                "error",
                bootfs_bzl,
                1,
                msg,
                rem,
            )


# Check Fuchsia Rust wrapper macro (.bzl) and bazel2gn converter invariants when modified.
def check_rust_wrapper_macro_hygiene():
    for ch_rel in sorted(changed):
        norm_rel = ch_rel.strip("/")
        is_new_rustc_macro = (
            norm_rel in newly_added_files
            and norm_rel.startswith("build/bazel/rules/rust/rustc_")
            and norm_rel.endswith(".bzl")
        )
        if (
            norm_rel == "build/bazel/rules/rust/rustc_staticlib.bzl"
            or is_new_rustc_macro
        ) and os.path.isfile(os.path.join(workdir, norm_rel)):
            bzl_txt = open(os.path.join(workdir, norm_rel), encoding="utf-8").read()
            _check_rustc_macro_bzl(norm_rel, bzl_txt)
        if (
            norm_rel.startswith("build/bazel/rules/packages/")
            and norm_rel.endswith(".bzl")
            and os.path.isfile(os.path.join(workdir, norm_rel))
        ):
            bzl_txt = open(os.path.join(workdir, norm_rel), encoding="utf-8").read()
            _check_bootfs_package_bzl(norm_rel, bzl_txt)
    _check_bootfs_package_tests_and_commit()
    _check_custom_cc_shared_library_macros()
    _check_rustc_macro_tests_and_gni()
    _check_bazel2gn_rust_staticlib_hygiene()
    _check_bazel2gn_test_macro_attr_parity()

    # Verify the commit message body does not claim removed wrapper features.
    st_bzl = "build/bazel/rules/rust/rustc_staticlib.bzl"
    if st_bzl in changed:
        msg_body = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
        if any(
            w in msg_body
            for w in ("LinkerInput", "TestCrateInfo", "non_test_deps", "hdrs (via CcInfo)")
        ):
            msg = (
                "Commit message body still describes removed "
                "`_rustc_staticlib_cc_wrapper` features (`hdrs`, `LinkerInput` "
                "re-owning, `non_test_deps` / `TestCrateInfo` filtering)."
            )
            rem = (
                "Amend the commit message body so it describes `rustc_staticlib` as a "
                "thin `rust_static_library` macro supporting `output_name`, "
                "`dynamic_libstd`, `gn_quiet_clippy`, `gn_disable_rustdoc`, "
                "and `validations`."
            )
            _emit(
                findings,
                "commit_body_stale_staticlib_wrapper_claims",
                "error",
                st_bzl,
                1,
                msg,
                rem,
            )


check_rust_wrapper_macro_hygiene()

# 4a. Reject newly added private external rule loads (@...//.../private/...) or duplicate
#     loads of the same rule symbol from both a public and a private .bzl location.
for rel in sorted(starlark_rels):
    try:
        with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
            st_src = f.read()
        st_tree = ast.parse(st_src, filename=rel)
    except Exception:
        continue
    base_st_flat = re.sub(r"\s+", "", "".join(git_lines(["show", f"{change_base}:{rel}"])))
    loaded_symbols = {}
    for stmt in st_tree.body:
        if not (
            isinstance(stmt, ast.Expr)
            and isinstance(stmt.value, ast.Call)
            and isinstance(stmt.value.func, ast.Name)
            and stmt.value.func.id == "load"
            and stmt.value.args
            and isinstance(stmt.value.args[0], ast.Constant)
            and isinstance(stmt.value.args[0].value, str)
        ):
            continue
        call = stmt.value
        mod_label = call.args[0].value
        seg = ast.get_source_segment(st_src, stmt) or ""
        is_new_load = not (seg and re.sub(r"\s+", "", seg) in base_st_flat)
        syms = []
        for a in call.args[1:]:
            if isinstance(a, ast.Constant) and isinstance(a.value, str):
                syms.append((a.value, a.value))
        for kw in call.keywords:
            if kw.arg and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                syms.append((kw.arg, kw.value.value))
        is_ext_private = bool(re.search(r"^@[^/]+//(?:[^:]*/)?private(?:/|:)", mod_label))
        for _local_name, orig_name in syms:
            prev_mod = loaded_symbols.get(orig_name)
            if is_new_load and (
                is_ext_private
                or (prev_mod and prev_mod != mod_label and "private" in (mod_label + prev_mod))
            ):
                dup_note = f" (duplicating `{orig_name}` already loaded from `{prev_mod}`)" if prev_mod else ""
                _emit(findings, "private_or_duplicate_rule_load", "error", rel, stmt.lineno, f"'{rel}' loads `{orig_name}` from private rule location `{mod_label}`{dup_note}. "
                        "Starlark files must only load rules from their public `.bzl` entry points.", f"Remove `load(\"{mod_label}\", ...)` from '{rel}' and use the public rule entry point instead.")
            loaded_symbols.setdefault(orig_name, mod_label)

# Detect .bzl files shared with GN: a .gni/.gn file `import()`s them, so they must stay valid
# GN syntax, where a triple-quoted Starlark docstring breaks `gn gen`.
_gn_imported_bzl_cache = {}


def bzl_imported_by_gn(rel):
    if not rel.endswith(".bzl"):
        return False
    if rel in _gn_imported_bzl_cache:
        return _gn_imported_bzl_cache[rel]
    base = os.path.basename(rel)
    found = False
    try:
        proc = subprocess.run(
            ["git", "grep", "-l", "-E", r"import\(\"[^\"]*" + re.escape(base) + r"\"\)",
             "--", "*.gni", "*.gn", "*/BUILD.gn"],
            cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
            timeout=60,
        )
        for gn_rel in proc.stdout.split():
            try:
                with open(os.path.join(workdir, gn_rel), "r", encoding="utf-8") as f:
                    gn_txt = f.read()
            except Exception:
                continue
            for m in re.finditer(r'import\("([^"]+)"\)', gn_txt):
                p = m.group(1)
                resolved = p[2:] if p.startswith("//") else os.path.normpath(
                    os.path.join(os.path.dirname(gn_rel), p))
                if resolved == rel:
                    found = True
                    break
            if found:
                break
    except Exception:
        found = False
    _gn_imported_bzl_cache[rel] = found
    return found


def head_txt_full(rel):
    try:
        with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
            return f.read(4096)
    except Exception:
        return ""


GN_SHARED_DOCSTRING_REMEDIATION = (
    "'{f}' is import()ed by GN ({{.gni,.gn}} files), so a triple-quoted docstring would break\n"
    "`gn gen`. Instead add `# buildifier: disable=module-docstring` on its own line right after\n"
    "the copyright header, with a comment saying the file is shared with GN."
)

buildifier_bin = None
for cand_rel in (
    "prebuilt/third_party/buildifier/linux-x64/buildifier",
    "prebuilt/third_party/buildifier/linux-arm64/buildifier",
    "prebuilt/third_party/buildifier/mac-x64/buildifier",
    "prebuilt/third_party/buildifier/mac-arm64/buildifier",
):
    cand_abs = os.path.join(workdir, cand_rel)
    if os.path.isfile(cand_abs) and os.access(cand_abs, os.X_OK):
        buildifier_bin = cand_abs
        break
if not buildifier_bin:
    buildifier_bin = shutil.which("buildifier")

buildifier_ran = False
if buildifier_bin and starlark_rels:
    try:
        proc = subprocess.run(
            [buildifier_bin, "-lint=warn", "-format=json", "-mode=check"] + sorted(starlark_rels),
            cwd=workdir,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        data = json.loads(proc.stdout) if proc.stdout.strip() else None
        if isinstance(data, dict) and "files" in data:
            buildifier_ran = True
            for file_res in data.get("files", []):
                fname = file_res.get("filename", "")
                if not file_res.get("valid", True):
                    _emit(findings, "buildifier_syntax_error", "error", fname, 1, f"Starlark file '{fname}' has a syntax error reported by buildifier.", f"Fix the Starlark syntax error in '{fname}'.")
                    continue
                if not file_res.get("formatted", True):
                    _emit(findings, "buildifier_format", "error", fname, 1, f"Starlark file '{fname}' is not formatted according to buildifier.", f"Format '{fname}' with `prebuilt/third_party/buildifier/linux-x64/buildifier {fname}` (or `fx format-code`).")
                is_autogen = False
                try:
                    with open(os.path.join(workdir, fname), "r", encoding="utf-8") as f:
                        head_txt = f.read(2048)
                    is_autogen = bool(re.search(r"AUTO-GENERATED|DO NOT EDIT", head_txt, re.I))
                except Exception:
                    pass
                for w in file_res.get("warnings", []):
                    w_cat = w.get("category", "buildifier_lint")
                    if w_cat == "module-docstring" and is_autogen:
                        continue
                    # A GN-shared .bzl may suppress module-docstring with a comment (the only
                    # GN-compatible form), which buildifier may still report: accept it.
                    if (
                        w_cat == "module-docstring"
                        and re.search(r"#\s*buildifier:\s*disable=module-docstring", head_txt_full(fname))
                        and bzl_imported_by_gn(fname)
                    ):
                        continue
                    w_line = w.get("start", {}).get("line", 1) or 1
                    w_msg = (w.get("message") or "").strip()
                    if w_cat == "module-docstring" and bzl_imported_by_gn(fname):
                        rem = GN_SHARED_DOCSTRING_REMEDIATION.format(f=fname)
                    elif w_cat == "module-docstring":
                        rem = (
                            f"Add a top-level module docstring (`\"\"\"...\"\"\"` string literal, not a `#` comment) "
                            f"as the first statement of '{fname}' immediately after the copyright header comments "
                            f"(before any `load()` or assignments), then format with "
                            f"`prebuilt/third_party/buildifier/linux-x64/buildifier {fname}`."
                        )
                    else:
                        rem = (
                            f"Fix the `{w_cat}` buildifier_lint finding in '{fname}' and verify with "
                            f"`prebuilt/third_party/buildifier/linux-x64/buildifier -lint=warn -mode=check {fname}`."
                        )
                    _emit(findings, "buildifier_lint", "error", fname, w_line, f"buildifier_lint ({w_cat}): {w_msg}", rem)
    except Exception:
        buildifier_ran = False

if not buildifier_ran:
    for rel in sorted(starlark_rels):
        if not rel.endswith(".bzl"):
            continue
        try:
            with open(os.path.join(workdir, rel), "r", encoding="utf-8") as f:
                src = f.read()
            if re.search(r"AUTO-GENERATED|DO NOT EDIT", src[:2048], re.I):
                continue
            tree = ast.parse(src, filename=rel)
        except Exception:
            continue
        if re.search(r"#\s*buildifier:\s*disable=module-docstring", src) and bzl_imported_by_gn(rel):
            continue
        if tree.body and ast.get_docstring(tree, clean=False) is None:
            if bzl_imported_by_gn(rel):
                fallback_rem = GN_SHARED_DOCSTRING_REMEDIATION.format(f=rel)
            else:
                fallback_rem = (
                    f"Add a top-level module docstring (`\"\"\"...\"\"\"` string literal, not a `#` "
                    f"comment) as the first statement of '{rel}' immediately after the copyright "
                    "header comments (before any `load()` or assignments)."
                )
            _emit(findings, "buildifier_lint", "error", rel, tree.body[0].lineno, "buildifier_lint (module-docstring): The file has no module docstring.\n"
                    "A module docstring is a string literal (not a comment) which should be the first "
                    "statement of a file (it may follow comment lines).", fallback_rem)

print(json.dumps(findings, indent=2))
PYEOF
