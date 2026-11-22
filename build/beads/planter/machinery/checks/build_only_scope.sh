#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -euo pipefail

# Deterministic check enforcing that a GN-to-Bazel migration only touches
# build-definition files. Any modification to source files (.rs, .cc, .h, .py,
# .go, .fidl, .cml, .json5, etc.) in the migration change is a scope violation:
# migrations must be pure build-graph refactors with zero behavioral/source diff.
#
# Files inspected: `git diff $PLANTER_CHANGE_BASE` (the task's commit, if HEAD is
# the task's commit, plus uncommitted changes) plus untracked files.
#
# Allowed paths (build-definition surface):
#   - BUILD.gn, BUILD.bazel, BUILD, *.BUILD.bazel, *.BUILD (any directory, including
#     workspace/repo root build files such as build/bazel/toplevel.BUILD.bazel)
#   - *.gni, *.bzl (build registration lists / macros, e.g. verification lists)
#   - MODULE.bazel, *.MODULE.bazel, WORKSPACE*, *.bazelrc
#   - a newly added (not modified) test component manifest `.cml` that an
#     `fx_component_manifest` of the nearest BUILD.bazel uses as `manifest` next to an
#     `fx_test_component` (it replaces the manifest GN's fuchsia_unittest_package generated;
#     Bazel does not generate test manifests, for C++ or Rust tests)
#   - a `.cml` manifest relocated byte-identically from an out-of-package path (such as an
#     unmigrated ancestor/sibling `meta/` directory referenced by the pre-migration `BUILD.gn`)
#     into a migrated Bazel package whose `BUILD.bazel` references it directly (including
#     removal of the pre-migration `.cml` path once no external GN targets use it)
#   - an existing `<pkg>/README.md` in a migrated package directory when every modified
#     (`+`/`-`) line only updates a target label reference (`//<pkg>...`) to match the
#     migrated BUILD.gn / BUILD.bazel targets; other README.md or OWNERS edits are not allowed.
#   - an existing `docs/**/*.md` file when every changed line is an `{% includecode %}`
#     directive whose `gerrit_path` only moves from a `BUILD.gn` region no longer present to
#     the same directory's `BUILD.bazel` carrying that `region_tag`
#     (`pcl.is_includecode_repoint_edit`); any other docs edit is still an error.
#   - an existing source file in a migrated package when modified solely to add
#     `LINT.IfChange` / `LINT.ThenChange` comments paired with `BUILD.bazel` / `BUILD.gn`,
#     and/or (in `.rs` files) to resolve a `_CLIPPY_WARN_PRODUCTION` lint (such as
#     `clippy::redundant_clone` or `#[allow(clippy::<lint>)]`) surfaced when
#     `//build/config/rust/lints:clippy_warn_all` is applied to a Bazel Rust test target.
#   - test-only code (`#[cfg(test)]` blocks in `.rs`, `<pkg>/tests/**`, or `*_test.*` /
#     `test_*.*` files) in a migrated package with `host_test_data_files` / `host_test_data_map`
#     when modified solely to adapt GN working-directory (`../../<pkg>/...`) or
#     executable-relative (`current_exe()`) test data paths to Bazel's `<test>.runtime_dir`.
#   - bazel2gn conversion tool sources/tests (`build/tools/bazel2gn/`, `build/bazel2gn/`).
#   - when the change adds or modifies a `.bzl`/`.gni` rule: shared Bazel scripts and
#     Starlark cqueries (`build/bazel/scripts/`, `build/bazel/starlark/`), rule verification
#     tests/fixtures under a `tests`/`test` package whose BUILD file also changed, files tied
#     by `LINT.IfChange`/`LINT.ThenChange` to another changed file, and `*_test.*`/`test_*.*`
#     unit tests next to a changed sibling file.
#   - an existing scrutiny golden (`.txt`/`.json5`) in a `goldens/` package with a scrutiny
#     BUILD rule, when a changed build file defines an assembly/bootfs package cutover.

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$WORKDIR" "$TARGET_DIRS" "$SCRIPT_DIR" <<'PYEOF'
import json
import os
import re
import shutil
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
raw_dirs = sys.argv[2]
sys.path.insert(0, os.path.abspath(sys.argv[3]))
import planter_check_lib as pcl
target_dirs = [
    d.strip().strip("/") for d in re.split(r"[\s,]+", raw_dirs) if d.strip().strip("/")
]

ALLOWED_BASENAMES = {"BUILD.gn", "BUILD.bazel", "BUILD", "MODULE.bazel"}
ALLOWED_SUFFIXES = (".gni", ".bzl", ".bazelrc", ".BUILD.bazel", ".BUILD", ".MODULE.bazel")
ALLOWED_PREFIX_BASENAMES = ("WORKSPACE",)
SOURCE_SUFFIXES = (".rs", ".cc", ".cpp", ".c", ".h", ".hpp")
CPP_SOURCE_SUFFIXES = (".c", ".cc", ".cpp", ".h", ".hh", ".hpp")
LINT_SUPPRESSION_ADD_RE = re.compile(
    r"#!?\[\s*(?:allow|expect)\s*\(|NOLINT|clippy::|^\s*let\s+_\s*="
)


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


changed = set()
# Resolve target package directories from environment overrides or CLI arguments
raw_tdirs = re.split(
    r"[\s,]+",
    (
        os.environ.get("PLANTER_TARGET_DIRS", "")
        or os.environ.get("PLANTER_TARGET_DIR", "")
        or raw_dirs
    ).strip(),
)
check_dirs = []
for d in raw_tdirs:
    nd = d.strip().strip("/")
    if nd:
        check_dirs.append(nd)
        if nd.startswith("build/secondary/"):
            check_dirs.append(nd[len("build/secondary/"):])
# The task's change: everything since the resolved change base ("HEAD~1" when HEAD is the task's
# own commit, "HEAD" when HEAD is unrelated upstream history or a stacked prerequisite commit).
change_base = pcl.resolve_change_base(workdir, target_dirs=check_dirs)
# Pass --no-renames so moved files deterministically appear in both added and deleted sets.
# Fuchsia .gitmodules sets `ignore = all`, which hides 160000 submodule gitlink changes from
# default `git diff`. Query `--cached --ignore-submodules=none` against `change_base` as well
# so staged or committed gitlink edits/deletions are caught without flagging unupdated working-tree
# submodule checkouts.
changed.update(git_lines(["diff", "--no-renames", "--name-only", change_base]))
changed.update(
    git_lines(
        ["diff", "--cached", "--no-renames", "--ignore-submodules=none", "--name-only", change_base]
    )
)


# Skip untracked agent report files at the checkout root.
# Coding agents sometimes write their CoderReport JSON (CoderReport*.json) to the checkout
# root. An untracked report is not part of the commit; in a parallel task another coder may
# have written it, and the coder that sees the finding is not allowed to delete it. It is only
# reported (INFO) so it is not committed; a committed copy still fails as a non-build file.
AGENT_REPORT_RE = re.compile(r"CoderReport[\w.-]*\.json")


def untracked_change_files():
    files = git_lines(["ls-files", "--others", "--exclude-standard"])
    reports = {f for f in files if "/" not in f and AGENT_REPORT_RE.fullmatch(f)}
    return [f for f in files if f not in reports], sorted(reports)


untracked_files, untracked_agent_reports = untracked_change_files()
changed.update(untracked_files)


def is_build_file(path: str) -> bool:
    base = os.path.basename(path)
    if base in ALLOWED_BASENAMES:
        return True
    if base.endswith(ALLOWED_SUFFIXES):
        return True
    if base.startswith(ALLOWED_PREFIX_BASENAMES):
        return True
    return False


# Newly added and deleted files (not in-place modifications of existing ones).
added = set(
    git_lines(["diff", "--no-renames", "--name-only", "--diff-filter=A", change_base])
)
added.update(
    git_lines(
        [
            "diff",
            "--cached",
            "--no-renames",
            "--ignore-submodules=none",
            "--name-only",
            "--diff-filter=A",
            change_base,
        ]
    )
)
added.update(untracked_files)
deleted = set(
    git_lines(["diff", "--no-renames", "--name-only", "--diff-filter=D", change_base])
)
deleted.update(
    git_lines(
        [
            "diff",
            "--cached",
            "--no-renames",
            "--ignore-submodules=none",
            "--name-only",
            "--diff-filter=D",
            change_base,
        ]
    )
)
deleted.update(p for p in changed if not os.path.exists(os.path.join(workdir, p)))


def git_blob_bytes(ref: str, rel_path: str):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir, "show", f"{ref}:{rel_path}"],
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        return None


def find_nearest_bazel_package(path: str):
    pkg = os.path.dirname(path)
    for _ in range(6):
        bazel = os.path.join(workdir, pkg, "BUILD.bazel")
        if os.path.isfile(bazel):
            try:
                with open(bazel, encoding="utf-8") as f:
                    return pkg, f.read()
            except OSError:
                return None, ""
        if not pkg:
            break
        pkg = os.path.dirname(pkg)
    return None, ""


# Identify .cml manifests relocated byte-identically from an out-of-package directory into
# their owning Bazel package (supporting both single-commit git mv and staged subpackage
# copies before outside_changes deletes the old path).
def find_relocated_component_manifests():
    new_to_old = {}
    old_to_new = {}
    refs = [change_base, "HEAD~1"] if change_base == "HEAD" else [change_base]
    for new_path in sorted(added):
        if not new_path.endswith(".cml"):
            continue
        full_new = os.path.join(workdir, new_path)
        if not os.path.isfile(full_new):
            continue
        pkg, bazel_text = find_nearest_bazel_package(new_path)
        if not pkg:
            continue
        rel_in_pkg = os.path.relpath(new_path, pkg)
        if not re.search(r'"(?::)?' + re.escape(rel_in_pkg) + r'"', bazel_text):
            continue
        try:
            with open(full_new, "rb") as f:
                new_bytes = f.read()
        except OSError:
            continue
        candidates = []
        for ref in refs:
            base_gn_raw = git_blob_bytes(ref, f"{pkg}/BUILD.gn")
            if base_gn_raw is not None:
                code_gn = re.sub(
                    r"#[^\n]*", "", base_gn_raw.decode("utf-8", errors="replace")
                )
                for raw_cml in re.findall(r'"([^"\n]+\.cml)"', code_gn):
                    cand_old = (
                        raw_cml[2:].lstrip("/")
                        if raw_cml.startswith("//")
                        else os.path.normpath(os.path.join(pkg, raw_cml)).replace("\\", "/")
                    )
                    if (
                        cand_old
                        and not cand_old.startswith("../")
                        and cand_old != new_path
                        and not cand_old.startswith(pkg + "/")
                    ):
                        candidates.append((ref, cand_old))
            for d_path in sorted(deleted):
                if (
                    d_path.endswith(".cml")
                    and d_path != new_path
                    and not d_path.startswith(pkg + "/")
                ):
                    candidates.append((ref, d_path))
        seen_cands = set()
        for ref, cand_old in candidates:
            if (ref, cand_old) in seen_cands:
                continue
            seen_cands.add((ref, cand_old))
            old_bytes = git_blob_bytes(ref, cand_old)
            if old_bytes is not None and old_bytes == new_bytes:
                new_to_old[new_path] = (cand_old, pkg)
                old_to_new.setdefault(cand_old, []).append((new_path, pkg))
                break
    return new_to_old, old_to_new


_bos_origin = git_out(["rev-parse", "--verify", "--quiet", "origin/main^{commit}"])


# Return True when `path` is a non-shard `.cml` file newly introduced by this branch
# (either added since `change_base` or absent on `origin/main` across multi-round amends).
def is_cl_added_cml(path: str) -> bool:
    if path in deleted or not path.endswith(".cml") or path.endswith(".shard.cml"):
        return False
    if not os.path.isfile(os.path.join(workdir, path)):
        return False
    if path in added:
        return True
    return bool(_bos_origin and git_blob_bytes(_bos_origin, path) is None)


# Check whether a newly added `.cml` is referenced as `manifest` in the nearest
# BUILD.bazel alongside an `fx_test_component` or `fx_unittest_component` target.
def is_new_test_manifest(path: str) -> bool:
    """A new `.cml` that an `fx_component_manifest` or `fx_unittest_component` in the nearest
    BUILD.bazel uses as `manifest` while that BUILD.bazel defines an `fx_test_component` or
    `fx_unittest_component`: it replaces the manifest that GN's `fuchsia_unittest_package`,
    `fuchsia_unittest_component`, or `test_component_manifest_cml` metadata generated."""
    if not is_cl_added_cml(path):
        return False
    pkg = os.path.dirname(path)
    for _ in range(4):
        bazel = os.path.join(workdir, pkg, "BUILD.bazel")
        if os.path.isfile(bazel):
            try:
                text = open(bazel, encoding="utf-8").read()
            except OSError:
                return False
            rel = (os.path.relpath(path, pkg) if pkg else path).replace("\\", "/")
            has_test_comp = (
                "fx_test_component(" in text or "fx_unittest_component(" in text
            )
            pkg_prefix = f"|//{re.escape(pkg)}:" if pkg else ""
            manifest_pat = rf'\bmanifest\s*=\s*"(?::{pkg_prefix})?{re.escape(rel)}"'
            return has_test_comp and bool(re.search(manifest_pat, text))
        if not pkg:
            break
        pkg = os.path.dirname(pkg)
    return False


relocated_new_to_old, relocated_old_to_new = find_relocated_component_manifests()


# Check whether an existing source file inside a migrated package only adds
# `LINT.IfChange` / `LINT.ThenChange` comments (paired with BUILD.bazel/BUILD.gn)
# and/or resolves a `_CLIPPY_WARN_PRODUCTION` lint (such as `redundant_clone`)
# surfaced when `//build/config/rust/lints:clippy_warn_all` is applied to a
# Bazel Rust test target.
IFTTT_COMMENT_RE = re.compile(r"^\s*(?://|#)\s*LINT\.(?:IfChange|ThenChange)\b")
COMMENT_CONT_RE = re.compile(r"^\s*(?://|#)")
CLIPPY_ALLOW_LINE_RE = re.compile(r"^\s*#!?\[\s*(?:allow|expect)\s*\(\s*clippy::")


# Strip single-line and multi-line LINT.IfChange / LINT.ThenChange comment blocks.
def strip_ifttt_comment_lines(lines):
    rem = []
    in_ifttt_paren = False
    for line in lines:
        if in_ifttt_paren and COMMENT_CONT_RE.match(line):
            if ")" in line:
                in_ifttt_paren = False
            continue
        in_ifttt_paren = False
        if IFTTT_COMMENT_RE.match(line):
            if "(" in line and ")" not in line:
                in_ifttt_paren = True
            continue
        rem.append(line)
    return rem


def check_allowed_ifttt_or_clippy_warn_all_test_edit(path: str):
    if path in added or not path.endswith(SOURCE_SUFFIXES):
        return None
    pkg, bzl_text = find_nearest_bazel_package(path)
    if not pkg:
        return None
    if f"{pkg}/BUILD.bazel" not in changed and f"{pkg}/BUILD.gn" not in changed:
        return None
    has_warn_all_test = (
        path.endswith(".rs")
        and "//build/config/rust/lints:clippy_warn_all" in bzl_text
        and any(
            tok in bzl_text
            for tok in (
                "rustc_test(",
                "host_rustc_test(",
                "with_unit_tests",
                "with_host_unit_tests",
            )
        )
    )
    diff_lines = git_lines(["diff", "-U0", change_base, "--", path])
    removed = []
    added_lines = []
    for raw in diff_lines:
        if raw.startswith(("diff ", "index ", "--- ", "+++ ", "@@ ")):
            continue
        if raw.startswith("-"):
            if raw[1:].strip():
                removed.append(raw[1:].strip())
        elif raw.startswith("+"):
            if raw[1:].strip():
                added_lines.append(raw[1:].strip())
    if not removed and not added_lines:
        return None
    rem_non_ifttt = strip_ifttt_comment_lines(removed)
    add_non_ifttt = strip_ifttt_comment_lines(added_lines)
    has_ifttt = len(rem_non_ifttt) < len(removed) or len(add_non_ifttt) < len(added_lines)
    if not rem_non_ifttt and not add_non_ifttt:
        if "LINT.IfChange" in bzl_text or "LINT.ThenChange" in bzl_text:
            return (
                f"Allowed LINT.IfChange/LINT.ThenChange path-sync comment update in '{path}' "
                f"paired with '//{pkg}/BUILD.bazel'."
            )
        return None
    if not has_warn_all_test:
        return None
    add_code = [l for l in add_non_ifttt if not CLIPPY_ALLOW_LINE_RE.match(l)]
    if not rem_non_ifttt and not add_code and len(add_non_ifttt) <= 4:
        return (
            f"Allowed clippy_warn_all test lint attribute in '{path}' for '//{pkg}'"
            + (" (with LINT.IfChange/LINT.ThenChange comments)." if has_ifttt else ".")
        )
    if (
        len(rem_non_ifttt) == len(add_code)
        and 0 < len(rem_non_ifttt) <= 4
        and all(
            r.replace(".clone()", "") == a
            for r, a in zip(rem_non_ifttt, add_code)
        )
    ):
        return (
            f"Allowed clippy_warn_all redundant_clone test lint fix in '{path}' for '//{pkg}'"
            + (" (with LINT.IfChange/LINT.ThenChange comments)." if has_ifttt else ".")
        )
    return None


# Mask Rust comments and string/char literals with spaces while preserving line numbers
# and brace positions so #[cfg(test)] / #[test] block line spans can be matched safely.
HOST_TEST_SOURCE_SUFFIXES = SOURCE_SUFFIXES + (".go", ".py")
RUST_MASK_RE = re.compile(
    r"//[^\n]*|/\*[\s\S]*?\*/|b?'(?:\\.|[^'\\\n])'|b?r(#*)\"[\s\S]*?\"\1"
    r"|b?\"(?:\\.|[^\"\\\n])*\""
)
RUST_TEST_ATTR_RE = re.compile(
    r"#\s*\[\s*(?:cfg\s*\(\s*test\s*\)|(?:\w+\s*::\s*)*test\b)"
)
HUNK_HDR_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@")
GN_TEST_DATA_PATH_RE = re.compile(
    r"\.\./|current_exe|os\.Executable|executable_path|__file__"
)


def _rust_cfg_test_line_ranges(text: str):
    masked = RUST_MASK_RE.sub(
        lambda m: "".join("\n" if ch == "\n" else " " for ch in m.group(0)), text
    )
    lines = masked.splitlines()
    spans = []
    i = 0
    n = len(lines)
    while i < n:
        if not RUST_TEST_ATTR_RE.search(lines[i]):
            i += 1
            continue
        start_ln = i + 1
        depth = 0
        opened = False
        j = i
        while j < n:
            for ch in lines[j]:
                if ch == "{":
                    depth += 1
                    opened = True
                elif ch == "}" and opened:
                    depth -= 1
                    if depth == 0:
                        spans.append((start_ln, j + 1))
                        break
                elif ch == ";" and not opened:
                    spans.append((start_ln, j + 1))
                    opened = True
                    depth = 0
                    break
            if opened and depth == 0:
                break
            j += 1
        else:
            if opened:
                spans.append((start_ln, n))
        i = max(i + 1, j + 1)
    return spans


# Check whether a modified source file only adapts test-scoped GN working-directory
# (`../../<pkg>/...`) or executable-relative (`current_exe()`) test data paths to
# Bazel's `<test>.runtime_dir` CWD in a package using `host_test_data_*`.
def check_allowed_host_test_data_path_edit(path: str):
    if path in added or not path.endswith(HOST_TEST_SOURCE_SUFFIXES):
        return None
    full_path = os.path.join(workdir, path)
    if not os.path.isfile(full_path):
        return None
    pkg, bzl_text = find_nearest_bazel_package(path)
    if not pkg:
        return None
    if f"{pkg}/BUILD.bazel" not in changed and f"{pkg}/BUILD.gn" not in changed:
        return None
    has_host_data_rule = bool(
        re.search(r"\b(?:host_test_data_files|host_test_data_map)\s*\(", bzl_text)
    )
    has_host_test_with_data = bool(
        re.search(
            r"\b(?:host_rustc_test|wrap_host_rust_test|host_go_test|host_py_test"
            r"|host_cc_test|host_test|with_host_unit_tests)\b",
            bzl_text,
        )
        and re.search(r"\b(?:data|test_data)\s*=", bzl_text)
    )
    if not (has_host_data_rule or has_host_test_with_data):
        return None
    diff_lines = git_lines(["diff", "-U0", change_base, "--", path])
    hunk_spans = []
    removed = []
    added_lines = []
    for raw in diff_lines:
        hm = HUNK_HDR_RE.match(raw)
        if hm:
            new_start = int(hm.group(1))
            new_count = int(hm.group(2)) if hm.group(2) is not None else 1
            if new_count == 0:
                pt = max(1, new_start)
                hunk_spans.append((pt, pt))
            else:
                hunk_spans.append((new_start, new_start + new_count - 1))
            continue
        if raw.startswith(("diff ", "index ", "--- ", "+++ ")):
            continue
        if raw.startswith("-"):
            body = raw[1:]
            if body.strip():
                removed.append(body.strip())
        elif raw.startswith("+"):
            body = raw[1:]
            if LINT_SUPPRESSION_ADD_RE.search(body):
                return None
            if body.strip():
                added_lines.append(body.strip())
    if not hunk_spans or (not removed and not added_lines):
        return None
    if len(removed) + len(added_lines) > 45:
        return None
    rel_in_pkg = os.path.relpath(path, pkg).replace("\\", "/")
    is_dedicated_test_file = bool(
        rel_in_pkg.startswith(("tests/", "test/"))
        or re.search(r"(?:^|/)test_[^/]+$|_test\.[A-Za-z0-9]+$", rel_in_pkg)
    )
    if path.endswith(".rs") and not is_dedicated_test_file:
        try:
            cur_src = open(full_path, encoding="utf-8").read()
        except OSError:
            return None
        cfg_spans = _rust_cfg_test_line_ranges(cur_src)
        if not cfg_spans or not all(
            any(s_lo <= h_lo and h_hi <= s_hi for s_lo, s_hi in cfg_spans)
            for h_lo, h_hi in hunk_spans
        ):
            return None
    elif not path.endswith(".rs") and not is_dedicated_test_file:
        return None
    diff_blob = "\n".join(removed + added_lines)
    if not GN_TEST_DATA_PATH_RE.search(diff_blob):
        return None
    data_tokens = {"test_data", "testdata", "TEST_SRCDIR", "RUNFILES"}
    for dm in re.finditer(r'\bdest_dir\s*=\s*"([^"]+)"', bzl_text):
        tok = dm.group(1).strip().strip("/")
        if tok:
            data_tokens.add(tok)
            data_tokens.add(tok.split("/", 1)[0])
    if not any(tok in diff_blob for tok in data_tokens):
        return None
    return (
        f"Allowed test-only runtime test-data path adaptation in '{path}' for '//{pkg}': "
        "Bazel host tests execute in '<test>.runtime_dir' where 'host_test_data_files' / "
        "'host_test_data_map' stages files under 'dest_dir' rather than GN's 'out/<dir>' "
        "CWD ('../../<pkg>/...') or executable parent directory. Use a single consistent "
        "'<dest_dir>/...' path (never runtime '.exists()' fallback probing), mark the "
        "Bazel host test '# @bazel2gn:skip', and explain the adaptation in the commit body."
    )


# Check whether an existing README.md inside a migrated package directory only updates
# target label references (`//<pkg>...`) to match the migrated BUILD.gn/BUILD.bazel targets.
def is_package_readme_label_update(path: str) -> bool:
    if path in added or os.path.basename(path) != "README.md":
        return False
    pkg = os.path.dirname(path).strip("/")
    if not pkg or not os.path.isfile(os.path.join(workdir, pkg, "BUILD.bazel")):
        return False
    if f"{pkg}/BUILD.bazel" not in changed and f"{pkg}/BUILD.gn" not in changed:
        return False
    diff_lines = git_lines(["diff", "-U0", change_base, "--", path])
    mod_lines = [
        l[1:].strip()
        for l in diff_lines
        if (l.startswith("+") and not l.startswith("+++"))
        or (l.startswith("-") and not l.startswith("---"))
    ]
    return bool(mod_lines) and all(f"//{pkg}" in l for l in mod_lines)


# Check whether a modified C/C++ file only normalizes checkout-root-relative
# `#include <sdk/lib/...>` directives to `<lib/...>` (for `idk_cc_source_library`
# targets with `include_base = "//sdk"`, where GN's `-I//` allowed `<sdk/lib/...>`
# while Bazel exposes `<lib/...>` on `-isystem`) or removes unused
# `#include <fidl/.../cpp/*test_base.h>` directives (isolated in `testonly = True`
# `<fidl>_cpp_testing` targets in Bazel), plus clang-format include sorting.
INCLUDE_LINE_RE = re.compile(r'^#\s*include\s+[<"][^>"]+[>"]\s*(?://.*|/\*.*\*/)?$')
SDK_LIB_INCLUDE_RE = re.compile(r"^(\s*#\s*include\s+<)sdk/(lib/[^>]+>.*)$")
FIDL_TEST_BASE_INCLUDE_RE = re.compile(
    r"^#\s*include\s+<fidl/([^/>]+)/cpp/(?:driver/)?(?:wire_)?test_base\.h>"
    r"\s*(?://.*|/\*.*\*/)?$"
)
FIDL_STD_HEADER_INCLUDE_RE = re.compile(
    r"^#\s*include\s+<fidl/([^/>]+)/cpp/(?:driver/)?"
    r"(?:fidl|wire|natural_types|wire_types|common_types|markers)\.h>"
    r"\s*(?://.*|/\*.*\*/)?$"
)


def is_idk_include_path_fix(path: str) -> bool:
    if path in added or not path.endswith(CPP_SOURCE_SUFFIXES):
        return False
    full_path = os.path.join(workdir, path)
    if not os.path.isfile(full_path):
        return False
    try:
        diff_out = subprocess.check_output(
            ["git", "-C", workdir, "diff", "-U0", change_base, "--", path],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return False
    removed = []
    added_lines = []
    for raw in diff_out.splitlines():
        if raw.startswith(("diff ", "index ", "--- ", "+++ ", "@@ ")):
            continue
        if raw.startswith("-"):
            removed.append(raw[1:])
        elif raw.startswith("+"):
            added_lines.append(raw[1:])
    if not removed and not added_lines:
        return True
    for line in removed + added_lines:
        stripped = line.strip()
        if stripped and not INCLUDE_LINE_RE.match(stripped):
            return False
    removed_test_base_libs = {
        m.group(1)
        for l in removed
        if (m := FIDL_TEST_BASE_INCLUDE_RE.match(l.strip()))
    }
    if removed_test_base_libs:
        pkg, _ = find_nearest_bazel_package(path)
        if pkg and (f"{pkg}/BUILD.bazel" in changed or f"{pkg}/BUILD.gn" in changed):
            kept_removed = sorted(
                l.strip()
                for l in removed
                if l.strip() and not FIDL_TEST_BASE_INCLUDE_RE.match(l.strip())
            )
            kept_added = sorted(
                l.strip()
                for l in added_lines
                if l.strip()
                and not (
                    (m := FIDL_STD_HEADER_INCLUDE_RE.match(l.strip()))
                    and m.group(1) in removed_test_base_libs
                    and l.strip() not in kept_removed
                )
            )
            if kept_removed == kept_added:
                return True
    has_sdk_lib_rewrite = any(SDK_LIB_INCLUDE_RE.match(l.strip()) for l in removed)
    if not has_sdk_lib_rewrite:
        return False
    norm_removed = sorted(
        SDK_LIB_INCLUDE_RE.sub(r"\1\2", l.strip()) for l in removed if l.strip()
    )
    norm_added = sorted(
        SDK_LIB_INCLUDE_RE.sub(r"\1\2", l.strip()) for l in added_lines if l.strip()
    )
    return norm_removed == norm_added


# Verify that any modified C/C++ file with normalized `#include` directives is
# properly formatted with clang-format so Format/ClangFormat does not fail in CQ.
def find_clang_format() -> str:
    for plat in ("linux-x64", "mac-x64", "mac-arm64"):
        cand = os.path.join(
            workdir, "prebuilt", "third_party", "clang", plat, "bin", "clang-format"
        )
        if os.path.isfile(cand) and os.access(cand, os.X_OK):
            return cand
    return shutil.which("clang-format") or ""


def check_cpp_include_formatting(path: str):
    clang_format_bin = find_clang_format()
    if not clang_format_bin:
        return None
    full_path = os.path.join(workdir, path)
    try:
        current = open(full_path, encoding="utf-8").read()
        formatted = subprocess.check_output(
            [clang_format_bin, "--style=file", full_path],
            cwd=workdir,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return None
    if current == formatted:
        return None
    diff_line = 1
    for idx, (cur_l, fmt_l) in enumerate(
        zip(current.splitlines(), formatted.splitlines()), start=1
    ):
        if cur_l != fmt_l:
            diff_line = idx
            break
    msg = (
        f"File '{path}' is not formatted properly (Format/ClangFormat) after "
        "normalizing '<sdk/lib/...>' include path to '<lib/...>'."
    )
    remediation = (
        f"Run 'fx format-code --changed-lines --files={path}' "
        f"(or '{clang_format_bin} -i --style=file {path}') so all #include "
        "directives are sorted according to Fuchsia clang-format rules."
    )
    return {
        "source": "build_only_scope",
        "category": "unformatted_cpp_include_fix",
        "severity": "error",
        "file": path,
        "line": diff_line,
        "message": msg,
        "remediation": remediation,
    }
findings = []

# Note untracked agent report files that were left out of the scope check.
for path in untracked_agent_reports:
    findings.append({
        "source": "build_only_scope",
        "category": "untracked_agent_report_ignored",
        "severity": "INFO",
        "file": path,
        "line": 0,
        "message": (
            f"Ignored untracked agent report '{path}' at the checkout root: it is not part of "
            "the change. Do not commit it; print the CoderReport JSON in the final reply "
            "instead of writing it to a file in the checkout."
        ),
    })

# Check whether the change adds or modifies Starlark (.bzl) or GN (.gni) build rule files.
HAS_BUILD_RULE_FILE_CHANGE = any(p.endswith((".bzl", ".gni")) for p in changed)
IFTTT_THENCHANGE_RE = re.compile(r"LINT\.ThenChange\(\s*(?://)?([^:)\s]+)")


# Extract repository-relative partner file paths from LINT.ThenChange annotations.
def extract_ifttt_partner_paths(text: str):
    partners = set()
    for m in IFTTT_THENCHANGE_RE.finditer(text or ""):
        raw_p = m.group(1).strip().strip("/")
        if raw_p:
            partners.add(raw_p)
    return partners


# Collect all LINT.ThenChange target paths referenced across modified files in this change.
def collect_incoming_ifttt_partners():
    referenced = set()
    for ch_path in sorted(changed):
        full_ch = os.path.join(workdir, ch_path.strip("/"))
        if not os.path.isfile(full_ch):
            continue
        try:
            with open(full_ch, encoding="utf-8", errors="replace") as cf:
                referenced.update(extract_ifttt_partner_paths(cf.read()))
        except OSError:
            continue
    return referenced


INCOMING_IFTTT_PARTNERS = collect_incoming_ifttt_partners()


# Check whether a file is part of bazel2gn build-conversion infrastructure, a rule/template
# verification test or fixture paired with a .bzl/.gni rule change, or participates
# in a LINT.IfChange/LINT.ThenChange contract with another file modified in the same change.
def check_allowed_build_rule_infra_file(path: str):
    norm = path.strip("/")
    # Allow updates to bazel2gn build-conversion tool sources and tests.
    if norm.startswith(("build/tools/bazel2gn/", "build/bazel2gn/")):
        return (
            f"Allowed bazel2gn build-translation infrastructure update in '{path}'."
        )
    if not HAS_BUILD_RULE_FILE_CHANGE:
        return None
    # Allow shared Bazel build-system scripts and Starlark cqueries paired with rule updates.
    if norm.startswith(("build/bazel/scripts/", "build/bazel/starlark/")):
        return (
            f"Allowed shared Bazel build infrastructure update in '{path}' "
            "paired with modified .bzl/.gni build rule definitions."
        )
    parts = [seg for seg in norm.split("/") if seg]
    pkg_dir = os.path.dirname(norm)
    # 1. Verification test sources, scripts, and fixtures inside a `tests`/`test` package
    #    whose owning BUILD.bazel or BUILD.gn is modified alongside a new .bzl/.gni rule.
    if any(seg in ("tests", "test") for seg in parts[:-1]):
        cur = pkg_dir
        while cur:
            if f"{cur}/BUILD.bazel" in changed or f"{cur}/BUILD.gn" in changed:
                return (
                    f"Allowed build-rule verification test file '{path}' in package "
                    f"'//{cur}' paired with newly added .bzl/.gni rule definitions."
                )
            cur = os.path.dirname(cur)
    # 2. Files linked via `LINT.IfChange` / `LINT.ThenChange`, outgoing or incoming, to another
    #    modified file in the same change (e.g., synchronized build-graph helper scripts).
    full_p = os.path.join(workdir, norm)
    cur_text = ""
    if os.path.isfile(full_p):
        try:
            with open(full_p, encoding="utf-8", errors="replace") as f:
                cur_text = f.read()
        except OSError:
            cur_text = ""
    direct_partners = extract_ifttt_partner_paths(cur_text)
    if (direct_partners & changed) or norm in INCOMING_IFTTT_PARTNERS:
        return (
            f"Allowed LINT.IfChange/LINT.ThenChange synchronized build infrastructure "
            f"update in '{path}' paired with modified partner file(s)."
        )
    # 3. Unit test files (`*_test.*` / `test_*.*`) testing a modified script in the same
    #    directory that participates in the build-rule infrastructure update.
    base = os.path.basename(norm)
    if re.search(r"(?:^test_|_test\.[A-Za-z0-9]+$)", base) and pkg_dir:
        sibling_changed = [
            c for c in changed if os.path.dirname(c.strip("/")) == pkg_dir and c != norm
        ]
        if sibling_changed:
            return (
                f"Allowed unit test update '{path}' paired with modified sibling build "
                f"infrastructure file(s) in '{pkg_dir}'."
            )
    return None


# Regex matching assembly and bootfs rule or template invocations in modified build files.
ASSEMBLY_CUTOVER_RULE_RE = re.compile(
    r"\b(?:fx_bootfs_files_package|bootfs_files_for_assembly"
    r"|fuchsia_assembly_input_bundle|assembly_input_bundle)\s*\("
)


# Check whether any modified build file defines or updates an assembly or bootfs target.
def has_modified_assembly_cutover_build_file() -> bool:
    for ch_path in sorted(changed):
        if not ch_path.endswith(("BUILD.bazel", "BUILD.gn", ".bzl", ".gni")):
            continue
        ch_full = os.path.join(workdir, ch_path)
        if not os.path.isfile(ch_full):
            continue
        try:
            with open(ch_full, encoding="utf-8", errors="replace") as bf:
                ch_txt = bf.read()
        except OSError:
            continue
        if ASSEMBLY_CUTOVER_RULE_RE.search(ch_txt):
            return True
    return False


# Check whether a modified scrutiny golden file under a golden build package
# is paired with an assembly or bootfs package cutover in the same change.
def check_allowed_assembly_scrutiny_golden_edit(path: str):
    if path in added or path in deleted or not path.endswith((".txt", ".json5")):
        return None
    norm = path.strip("/")
    pkg_dir = os.path.dirname(norm).strip("/")
    if os.path.basename(pkg_dir) != "goldens":
        return None
    full_path = os.path.join(workdir, norm)
    if not os.path.isfile(full_path):
        return None
    goldens_bzl_text = ""
    for build_name in ("BUILD.bazel", "BUILD.gn"):
        cand_build = os.path.join(workdir, pkg_dir, build_name)
        if os.path.isfile(cand_build):
            try:
                with open(cand_build, encoding="utf-8", errors="replace") as f:
                    goldens_bzl_text += "\n" + f.read()
            except OSError:
                continue
    if not goldens_bzl_text.strip():
        return None
    base = os.path.basename(norm)
    has_scrutiny_rule = "scrutiny" in goldens_bzl_text
    has_file_ref = f'"{base}"' in goldens_bzl_text or "glob(" in goldens_bzl_text
    if not (has_scrutiny_rule and has_file_ref):
        return None
    if not has_modified_assembly_cutover_build_file():
        return None
    return (
        f"Allowed assembly scrutiny golden update in '{path}' paired with "
        "assembly/bootfs package cutover in BUILD.bazel."
    )


# Check whether a modified verification test file under `<area>/build/tests/**` only
# updates subtool/artifact list entries paired with cutting over a `gn_ffx_test_data`
# or `@gn_targets` bridge stub in a modified `BUILD.bazel` to native Bazel.
def check_allowed_gn_bridge_cutover_test_edit(path: str):
    if path in added or path in deleted or not path.endswith((".go", ".py", ".rs", ".sh")):
        return None
    norm = path.strip("/")
    full_norm = os.path.join(workdir, norm)
    if not os.path.isfile(full_norm):
        return None
    owner_pkg = os.path.dirname(norm).strip("/")
    while owner_pkg and not os.path.isfile(os.path.join(workdir, owner_pkg, "BUILD.bazel")):
        parent = os.path.dirname(owner_pkg).strip("/")
        if parent == owner_pkg:
            owner_pkg = ""
            break
        owner_pkg = parent
    if not owner_pkg or not re.search(r"(?:^|/)build/tests?(?:/|$)", owner_pkg):
        return None
    try:
        with open(os.path.join(workdir, owner_pkg, "BUILD.bazel"), encoding="utf-8") as f:
            owner_bz_txt = f.read()
    except OSError:
        return None
    rel_src = os.path.relpath(norm, owner_pkg).replace("\\", "/")
    if rel_src not in owner_bz_txt:
        return None
    bridge_re = re.compile(r"\bgn_ffx_test_data\b|@gn_targets//")
    changed_bz_pkgs = sorted(
        {
            os.path.dirname(p.strip("/"))
            for p in changed
            if os.path.basename(p) == "BUILD.bazel" and os.path.dirname(p.strip("/"))
        }
    )
    cutover_tds = []
    for td in changed_bz_pkgs:
        if not re.search(r"//" + re.escape(td) + r'(?=[:"/\s\'])', owner_bz_txt):
            continue
        td_bz_rel = f"{td}/BUILD.bazel"
        base_td_bz = git_out(["show", f"{change_base}:{td_bz_rel}"])
        if not base_td_bz or not bridge_re.search(base_td_bz):
            continue
        try:
            with open(os.path.join(workdir, td_bz_rel), encoding="utf-8") as f:
                cur_td_bz = f.read()
        except OSError:
            continue
        if not bridge_re.search(cur_td_bz):
            cutover_tds.append((td, base_td_bz, cur_td_bz))
    if not cutover_tds:
        return None
    diff_lines = git_lines(["diff", "-U0", change_base, "--", norm])
    removed = [
        ln[1:].strip()
        for ln in diff_lines
        if ln.startswith("-") and not ln.startswith("---") and ln[1:].strip()
    ]
    added_lines = [
        ln[1:].strip()
        for ln in diff_lines
        if ln.startswith("+") and not ln.startswith("+++") and ln[1:].strip()
    ]
    if not (removed or added_lines) or len(removed) + len(added_lines) > 20:
        return None
    ignored_vals = {"2021", "2024", "0.1.0", "partner", "internal", "host-tools"}
    cutover_tokens = set()
    for _td, base_td_bz, cur_td_bz in cutover_tds:
        for tok in re.findall(r'"([A-Za-z0-9_.-]+)"', base_td_bz + "\n" + cur_td_bz):
            if len(tok) >= 3 and tok not in ignored_vals:
                cutover_tokens.add(tok)
    if not cutover_tokens:
        return None
    combined = "\n".join(removed + added_lines)
    if not any(f'"{tok}"' in combined for tok in cutover_tokens):
        return None

    def _strip_literals(lines):
        blob = "\n".join(lines)
        for tok in sorted(cutover_tokens, key=len, reverse=True):
            blob = blob.replace(f'"{tok}"', "")
        return re.sub(r"[,\s]+", "", blob)

    if _strip_literals(removed) != _strip_literals(added_lines):
        return None
    return (
        f"Allowed GN-bridge cutover verification test update in '{path}' "
        f"(package '//{owner_pkg}') paired with replacing a GN bridge stub in BUILD.bazel."
    )


# Detect unfinished git rebase/merge/cherry-pick states and unmerged index paths.
def detect_unfinished_git_operation():
    active_state = None
    for state_name in ("rebase-merge", "rebase-apply", "MERGE_HEAD", "CHERRY_PICK_HEAD"):
        git_path = git_out(["rev-parse", "--git-path", state_name])
        if not git_path:
            continue
        abs_git_path = (
            git_path if os.path.isabs(git_path) else os.path.join(workdir, git_path)
        )
        if os.path.exists(abs_git_path):
            active_state = state_name
            break
    unmerged = set(git_lines(["diff", "--name-only", "--diff-filter=U"]))
    if not active_state and not unmerged:
        return None, unmerged
    sorted_unmerged = sorted(unmerged)
    state_desc = active_state or "unmerged index entries"
    suffix = (
        f" with unmerged file(s): {', '.join(sorted_unmerged)}"
        if sorted_unmerged
        else ""
    )
    finding = {
        "source": "build_only_scope",
        "category": "unfinished_git_operation",
        "severity": "error",
        "file": sorted_unmerged[0] if sorted_unmerged else "",
        "line": 0,
        "message": (
            f"An unfinished git operation (`{state_desc}`) is active in the "
            f"checkout{suffix}."
        ),
        "remediation": (
            "Resolve all conflicted files, stage them with `git add <file>`, and "
            "complete the operation with `git -c core.editor=true rebase --continue` "
            "(or abort with `git rebase --abort` / `git merge --abort`)."
        ),
    }
    return finding, unmerged


# Build remediation guidance for a newly added `.cml` not wired to a Bazel test component.
def build_unwired_new_cml_remediation(path: str) -> str:
    return (
        f"If '{path}' replaces a manifest generated by GN's `fuchsia_unittest_package` "
        "or `fuchsia_unittest_component`, reference it via `manifest = \"...\"` on an "
        "`fx_component_manifest` or `fx_unittest_component` alongside an "
        "`fx_test_component` or `fx_unittest_component` in the nearest `BUILD.bazel`. "
        f"Otherwise, remove '{path}' (`rm -f {path}`) and use existing checked-in "
        "component manifests; GN-to-Bazel migrations must not add new non-test `.cml` "
        "files."
    )


unfinished_op_finding, unmerged_paths = detect_unfinished_git_operation()
if unfinished_op_finding is not None:
    findings.append(unfinished_op_finding)

# Emit findings for changed non-build files and relocated manifests.
for path in sorted(changed):
    if unfinished_op_finding is not None and (
        path in unmerged_paths or is_cl_added_cml(path)
    ):
        continue
    if (
        is_build_file(path)
        or is_new_test_manifest(path)
        or is_package_readme_label_update(path)
        or pcl.is_includecode_repoint_edit(workdir, change_base, path)
    ):
        continue
    gn_bridge_test_note = check_allowed_gn_bridge_cutover_test_edit(path)
    if gn_bridge_test_note is not None:
        findings.append({
            "source": "build_only_scope",
            "category": "allowed_gn_bridge_cutover_test_edit",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": gn_bridge_test_note,
        })
        continue
    scrutiny_golden_note = check_allowed_assembly_scrutiny_golden_edit(path)
    if scrutiny_golden_note is not None:
        findings.append({
            "source": "build_only_scope",
            "category": "allowed_assembly_scrutiny_golden_update",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": scrutiny_golden_note,
        })
        continue
    rule_infra_note = check_allowed_build_rule_infra_file(path)
    if rule_infra_note is not None:
        findings.append({
            "source": "build_only_scope",
            "category": "allowed_build_rule_infra_file",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": rule_infra_note,
        })
        continue
    ifttt_or_clippy_note = check_allowed_ifttt_or_clippy_warn_all_test_edit(path)
    if ifttt_or_clippy_note is not None:
        findings.append({
            "source": "build_only_scope",
            "category": "allowed_ifttt_or_test_clippy_edit",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": ifttt_or_clippy_note,
        })
        continue
    host_test_data_note = check_allowed_host_test_data_path_edit(path)
    if host_test_data_note is not None:
        findings.append({
            "source": "build_only_scope",
            "category": "allowed_host_test_data_path_edit",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": host_test_data_note,
        })
        continue
    if path in relocated_new_to_old:
        old_p, owner_pkg = relocated_new_to_old[path]
        findings.append({
            "source": "build_only_scope",
            "category": "relocated_component_manifest",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": (
                f"Allowed byte-identical component manifest relocation: '{path}' is an "
                f"unchanged copy of '{old_p}' moved into Bazel package '//{owner_pkg}' "
                "so its manifest target owns the file directly."
            ),
        })
        continue
    if path in deleted and path in relocated_old_to_new:
        dest_desc = ", ".join(
            f"'{np}' (//{op})" for np, op in relocated_old_to_new[path]
        )
        findings.append({
            "source": "build_only_scope",
            "category": "relocated_component_manifest",
            "severity": "INFO",
            "file": path,
            "line": 0,
            "message": (
                f"Allowed removal of relocated component manifest '{path}', moved "
                f"byte-identically to {dest_desc}."
            ),
        })
        continue
    if is_idk_include_path_fix(path):
        fmt_finding = check_cpp_include_formatting(path)
        if fmt_finding is not None:
            findings.append(fmt_finding)
        continue
    if is_cl_added_cml(path):
        msg_text = (
            f"Newly added component manifest '{path}' is not paired with an "
            "`fx_test_component` or `fx_unittest_component` in the nearest `BUILD.bazel`."
        )
        rem_text = build_unwired_new_cml_remediation(path)
    else:
        msg_text = (
            f"Migration modifies non-build file '{path}'. GN-to-Bazel migrations must not "
            "change source code, tests, manifests, submodule gitlinks/symlinks, or data files "
            "(see //build/beads/references/migration/scoping_changelists.md)."
        )
        rem_text = (
            "Revert this path to its pre-migration contents (`git checkout HEAD~1 -- <file>` "
            "and `git commit --amend --no-edit` if committed in HEAD, `git checkout "
            "$PLANTER_CHANGE_BASE -- <file>` for unstaged files, or `git reset origin/main -- "
            "<submodule>` and `git commit --amend --no-edit` when restoring a 160000 submodule "
            "gitlink). Never stage or commit worktree symlinks that "
            "replace third-party submodule gitlinks during rebase or `git commit -a`, as "
            "replacing a gitlink with a symlink causes CQ `checkout.jiri update` to fail with "
            "`could not create work tree dir ...: File exists`. If the Bazel build surfaces "
            "lint or compile errors on a target built in GN, fix the BUILD.bazel attributes "
            "(lint_config, rustc_flags, configs, features, deps) instead of editing sources "
            "(for a C/C++ '#include <sdk/lib/...>' on an idk_cc_source_library with "
            "include_base = '//sdk', rewrite only that include to '<lib/...>' and run "
            "'fx format-code --changed-lines --files=<file>'; for a host test using "
            "host_test_data_files/host_test_data_map whose #[cfg(test)]/test code hardcodes "
            "'../../<pkg>/...' or current_exe(), keep the edit strictly inside "
            "#[cfg(test)]/test files to use a single consistent '<dest_dir>/...' path (never "
            "runtime '.exists()' fallback probing) and mark the Bazel host test "
            "'# @bazel2gn:skip'). "
            "If a target was not being built in GN and has pre-existing source bitrot (or may "
            "be dead code), do not fix source files in the migration CL: either omit an "
            "unreferenced unit test target with a `# TODO` comment in BUILD.bazel and note it "
            "in the commit message, or leave the broken target unmigrated so its code and GN "
            "build graph can be fixed (or deleted) in a separate precursor CL."
        )
    findings.append({
        "source": "build_only_scope",
        "category": "non_build_file_modified",
        "severity": "error",
        "file": path,
        "line": 0,
        "message": msg_text,
        "remediation": rem_text,
    })

print(json.dumps(findings, indent=2))
PYEOF
