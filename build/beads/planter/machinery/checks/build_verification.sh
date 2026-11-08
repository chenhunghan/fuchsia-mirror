#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Deterministic check that actually builds the migration. Static checks and the
# review panel only read BUILD files; this is what proves the change compiles.
#
# Steps (each failing step becomes one ERROR finding with the relevant log lines):
#   1. gn_build:               Target-scoped `fx build <labels>` for the changed
#                              packages and their dependents (falling back to bare
#                              `fx build` if no labels found, or if
#                              PLANTER_FULL_GN_BUILD=1).
#   2. gn_build_dependents:    `fx build <labels>` for any remaining GN dependents
#                              if gn_build ran a full product build. Skipped when
#                              gn_build is target-scoped or fails.
#   3. shac_check:             `fx lint --fail-level=warning`, which builds
#                              `//bundles/buildbot/static_checks` and runs shac
#                              static checks on the change (failing on both errors
#                              and warnings, including `verify_bazel2gn` for
#                              BUILD.gn/BUILD.bazel drift, buildifier, gn_format,
#                              fidl_gn2bazel_migration_check, and check_licenses).
#   4. bazel_build_fuchsia:    `fx bazel build --config=fuchsia_platform
#                              --noexpand_test_suites //<dir>:all`
#                              for every changed directory with a BUILD.bazel
#                              (host-only targets are skipped as incompatible,
#                              and `--noexpand_test_suites` prevents `:all` from
#                              expanding `test_suite` targets before checking
#                              their `target_compatible_with`;
#                              `_validate_json_action` targets such as
#                              `<fidl>_validate_ir_json` are excluded from top-level
#                              `:all` so `assert_no_deps_aspect` does not crash on
#                              their scalar `data` attribute, while `:<fidl>` still
#                              runs IR JSON validation via `_validation`).
#   5. bazel_build_host:       `fx bazel build --config=host //<dir>:all` for the
#                              directories whose BUILD.bazel declares host targets.
#
# Directories: $PLANTER_TARGET_DIRS plus every directory whose BUILD.bazel or
# BUILD.gn changed since $PLANTER_CHANGE_BASE (e.g. migrated dependencies).
# With several target directories planter runs checks once per directory; this
# check only builds on the run for the first one.
#
# Result reuse: a run whose builds all pass records a fingerprint of exactly
# what it built in <git dir>/planter-build-verification.json. A later run with
# the same fingerprint reuses that result instead of rebuilding (reported as an
# INFO finding), so planter does not repeat the coder's final `run_checks.sh`.
# The fingerprint covers this script, the git tree of the whole working tree
# (committed or not, including untracked files), the build directories and
# steps, args.gn, the last jiri update and the environment below. Failures are
# never reused, and a result expires after PLANTER_BUILD_REUSE_TTL seconds.
#
# Environment:
#   PLANTER_SKIP_BUILD=1          skip the builds (still validates commit Test:
#                                 footers and reports an INFO finding).
#   PLANTER_FULL_GN_BUILD=1       always run full product `fx build` instead of
#                                 target-scoped build.
#   PLANTER_BUILD_NO_REUSE=1      always build; do not reuse a recorded result.
#   PLANTER_BUILD_REUSE_TTL=<secs>  max age of a reused result (default 21600).
#   PLANTER_BUILD_DRY_RUN=1       run nothing; report the planned commands as
#                                 INFO findings.
#   PLANTER_BUILD_TIMEOUT=<secs>  per-step timeout (default 5400).
#   PLANTER_DEPENDENT_BUILD_LIMIT=<n>  max labels for gn_build_dependents
#                                 (default 40).

WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

norm_target_dir="${TARGET_DIR#//}"
norm_target_dir="${norm_target_dir#./}"
norm_target_dir="${norm_target_dir%/}"
first_dir="${TARGET_DIRS%%[ ,]*}"
first_dir="${first_dir#//}"
first_dir="${first_dir#./}"
first_dir="${first_dir%/}"
if [[ -n "$norm_target_dir" && -n "$first_dir" && "$norm_target_dir" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

python3 - "$WORKDIR" "$TARGET_DIRS" "${BASH_SOURCE[0]}" <<'PYEOF'
import ast
import fcntl
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(sys.argv[3])))
try:
    import planter_check_lib as _pcl
except ImportError:
    _pcl = None

# Parse whitespace- or comma-delimited target directory arguments into normalized
# checkout-relative paths while preserving order (`planter_check_lib.parse_target_dirs`,
# with an equivalent inline fallback when the library cannot be imported).
def _parse_target_dirs(raw_dirs, wdir):
    if _pcl is not None and hasattr(_pcl, "parse_target_dirs"):
        return _pcl.parse_target_dirs(raw_dirs, wdir)
    out = []
    for tok in re.split(r"[\s,]+", raw_dirs or ""):
        norm = re.sub(r"^(?://|\./)+", "", tok.strip()).strip("/")
        if norm == ".":
            norm = ""
        if norm and norm not in out:
            out.append(norm)
    return out


workdir = os.path.abspath(sys.argv[1])
target_dirs = _parse_target_dirs(sys.argv[2], workdir)
if _pcl is not None and hasattr(_pcl, "resolve_change_base"):
    change_base = _pcl.resolve_change_base(workdir)
else:
    change_base = (
        os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
        or "HEAD"
    )
timeout = int(os.environ.get("PLANTER_BUILD_TIMEOUT", "5400"))
dry_run = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
skip_build = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
dependent_limit = int(os.environ.get("PLANTER_DEPENDENT_BUILD_LIMIT", "40"))


def git_lines(args):
    try:
        out = subprocess.check_output(["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True)
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


# Serialize active builds against tests_json_parity working-tree reversion and `fx gen`
# runs so concurrent check invocations never collide in regenerator.py.
def acquire_workdir_build_lock(wdir):
    if skip_build or dry_run:
        return None
    try:
        gdir = subprocess.check_output(
            ["git", "-C", wdir, "rev-parse", "--absolute-git-dir"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        if not gdir or not os.path.isdir(gdir):
            return None
        lf = open(os.path.join(gdir, "planter-workdir-build.lock"), "a+", encoding="utf-8")
        os.set_inheritable(lf.fileno(), False)
        fcntl.flock(lf.fileno(), fcntl.LOCK_EX)
        return lf
    except (OSError, subprocess.CalledProcessError):
        return None


_workdir_build_lock = acquire_workdir_build_lock(workdir)


EXCLUDED_GLOBAL_DIRS = (
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
)

dirs = list(target_dirs)
env_changed = os.environ.get("PLANTER_CHANGED_FILES", "").strip()
changed = (
    [l.strip() for l in env_changed.splitlines() if l.strip()]
    if env_changed
    else git_lines(["diff", "--name-only", change_base]) + git_lines(["ls-files", "--others", "--exclude-standard"])
)
# Check whether a commit is already merged upstream (on origin/main, origin/master,
# jiri/head, or carrying a Gerrit Reviewed-on: footer).
def _is_upstream_commit(commit_sha):
    if not commit_sha:
        return False
    head_body = "\n".join(git_lines(["log", "-1", "--format=%B", commit_sha]))
    if re.search(r"(?m)^Reviewed-on:\s*\S", head_body):
        return True
    for ref_name in ("origin/main", "origin/master", "jiri/head"):
        ref_c = (git_lines(["rev-parse", "--verify", f"{ref_name}^{{commit}}"]) or [""])[0]
        if not ref_c:
            continue
        if ref_c == commit_sha:
            return True
        mb = (git_lines(["merge-base", ref_c, commit_sha]) or [""])[0]
        if mb and mb == commit_sha:
            return True
    return False


# Fall back to HEAD~1 only when HEAD is a local migration commit for target_dirs
# (never when HEAD is already merged upstream or belongs to a stacked parent).
if not changed:
    head_subj = (git_lines(["log", "-1", "--format=%s", "HEAD"]) or [""])[0]
    head_c = (git_lines(["rev-parse", "--verify", "HEAD^{commit}"]) or [""])[0]
    base_c = (git_lines(["rev-parse", "--verify", f"{change_base}^{{commit}}"]) or [""])[0]
    head_on_origin = _is_upstream_commit(head_c)
    head_files = git_lines(["diff", "--name-only", "HEAD~1", "HEAD"])
    head_touches_td = not target_dirs or any(
        any(p == td or p.startswith(td + "/") for td in target_dirs)
        for p in head_files
    )
    if (
        not head_on_origin
        and head_touches_td
        and (change_base == "HEAD" or (head_c and head_c == base_c))
        and head_subj.startswith(("[bazel_migration]", "[bazel2gn]", "[bazel]"))
    ):
        change_base = "HEAD~1"
        changed = head_files + git_lines(
            ["ls-files", "--others", "--exclude-standard"]
        )
changed_set = set(changed)


all_changed_build_dirs = {
    os.path.dirname(p).strip("/")
    for p in changed
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn") and os.path.dirname(p).strip("/")
}


# Check whether an ancestor directory of a target_dir (or of another changed BUILD directory
# when target_dirs is empty) was only modified to wire tests (test_suite in BUILD.bazel,
# bazel_test_suite or group("tests") in BUILD.gn) rather than migrating non-test targets
# of its own.
def _is_ancestor_test_wiring_only(d):
    descendant_dirs = target_dirs or all_changed_build_dirs
    if d in target_dirs or not any(td.startswith(d + "/") for td in descendant_dirs):
        return False
    for fn in ("BUILD.bazel", "BUILD.gn"):
        rel_f = f"{d}/{fn}"
        if rel_f not in changed_set:
            continue
        diff_txt = "\n".join(git_lines(["diff", "-U0", change_base, "--", rel_f]))
        for raw_ln in diff_txt.splitlines():
            if not raw_ln.startswith(("+", "-")) or raw_ln.startswith(("+++", "---")):
                continue
            s = raw_ln[1:].strip()
            if not s or s.startswith("#") or s in ("{", "}", "]", ")", "],", "),"):
                continue
            if fn == "BUILD.bazel":
                if re.match(
                    r'^(?:test_suite\s*\(|name\s*=\s*"(?:target_|host_)?tests"|'
                    r"tests\s*=\s*\[|target_compatible_with\s*=|"
                    r'"//[^"]+"[,]?$|load\()',
                    s,
                ):
                    continue
                return False
            else:
                if re.match(
                    r'^(?:import\("//build/bazel/bazel_test_suite\.gni"\)|'
                    r'bazel_test_suite\s*\(|group\(\s*"tests"\s*\)|'
                    r"testonly\s*=\s*true|(?:deps|host_tests|target_tests)\s*\+?=\s*\[|"
                    r'"[^"]+"[,]?$)',
                    s,
                ):
                    continue
                return False
    return True


# Normalize non-collateral top-level targets in a BUILD.bazel file so collateral-only edits
# (visibility, exports_files, package, load, test_suite, or HOST_CONSTRAINTS ->
# HOST_OS_CONSTRAINTS) do not trigger building `:all` or running pre-existing untouched
# host tests in non-target packages.
def _is_host_compat_value(node):
    return isinstance(node, ast.Name) and node.id in (
        "HOST_CONSTRAINTS",
        "HOST_OS_CONSTRAINTS",
    )


def _canon_bazel_non_collateral(src):
    tree = ast.parse(src)
    items = []
    for stmt in tree.body:
        if not (isinstance(stmt, ast.Expr) and isinstance(stmt.value, ast.Call)):
            items.append(ast.dump(stmt))
            continue
        call = stmt.value
        fn = (
            call.func.id
            if isinstance(call.func, ast.Name)
            else (call.func.attr if isinstance(call.func, ast.Attribute) else "")
        )
        if fn in ("load", "package", "exports_files", "licenses", "test_suite"):
            continue
        kept_kws = sorted(
            (
                kw.arg or "",
                "HOST_COMPAT"
                if (kw.arg == "target_compatible_with" and _is_host_compat_value(kw.value))
                else ast.dump(kw.value),
            )
            for kw in call.keywords
            if kw.arg != "visibility"
        )
        items.append((fn, [ast.dump(a) for a in call.args], kept_kws))
    return items


# Check whether a package is a target directory, newly created, or modified build/test
# targets beyond collateral `visibility`, `exports_files`, `test_suite`, or
# `HOST_CONSTRAINTS` -> `HOST_OS_CONSTRAINTS` edits.
def _has_substantive_bazel_target_change(d):
    if d in target_dirs:
        return True
    try:
        base_src = subprocess.check_output(
            ["git", "-C", workdir, "show", f"{change_base}:{d}/BUILD.bazel"],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return True
    try:
        with open(
            os.path.join(workdir, d, "BUILD.bazel"), encoding="utf-8", errors="replace"
        ) as f:
            cur_src = f.read()
        return _canon_bazel_non_collateral(base_src) != _canon_bazel_non_collateral(cur_src)
    except Exception:
        return True


# Check whether a changed file outside the target dirs is owned by a stacked parent commit:
# the working tree leaves it as committed at HEAD, and every commit since the change base that
# touched it touched none of the target dirs. Syncing such files (e.g. `fx bazel2gn` on the
# parent's packages) belongs in the parent commit; doing it here makes the review diff report
# the parent's targets as unmatched, so shac findings in them must not block this change.
_stacked_parent_cache = {}


def _existed_at_change_base(p):
    try:
        return (
            subprocess.run(
                ["git", "-C", workdir, "cat-file", "-e", f"{change_base}:{p}"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            ).returncode
            == 0
        )
    except Exception:
        return False


# Find the nearest enclosing Bazel/GN package directory for a workspace-relative path.
def _owning_pkg_dir(rel_p):
    cur = os.path.dirname(rel_p).strip("/")
    while cur:
        if (
            os.path.isfile(os.path.join(workdir, cur, "BUILD.bazel"))
            or os.path.isfile(os.path.join(workdir, cur, "BUILD.gn"))
            or _existed_at_change_base(f"{cur}/BUILD.bazel")
            or _existed_at_change_base(f"{cur}/BUILD.gn")
        ):
            return cur
        cur = os.path.dirname(cur).strip("/")
    return ""


# Check whether workspace-relative path `p` belongs directly to one of `pkg_dirs`
# rather than to a nested child subpackage with its own BUILD.bazel/BUILD.gn.
def _path_in_pkg_dirs(p, pkg_dirs):
    norm_p = os.path.normpath(p).strip("/")
    if not norm_p or not pkg_dirs:
        return False
    if norm_p in pkg_dirs:
        return True
    owner = _owning_pkg_dir(norm_p)
    if owner:
        return owner in pkg_dirs
    return any(norm_p == d or norm_p.startswith(d + "/") for d in pkg_dirs)


def _commit_touches_target_dirs(sha):
    files = git_lines(["diff-tree", "--no-commit-id", "--name-only", "-r", sha])
    return any(_path_in_pkg_dirs(f, target_dirs) for f in files)


def _owned_by_stacked_parent_commit(p):
    if p in _stacked_parent_cache:
        return _stacked_parent_cache[p]
    owned = False
    in_target = _path_in_pkg_dirs(p, target_dirs)
    if target_dirs and change_base != "HEAD" and not in_target:
        dirty = git_lines(["diff", "--name-only", "HEAD", "--", p])
        untracked = git_lines(["ls-files", "--others", "--exclude-standard", "--", p])
        commits = git_lines(["log", "--format=%H", f"{change_base}..HEAD", "--", p])
        owned = (
            not dirty
            and not untracked
            and bool(commits)
            and not any(_commit_touches_target_dirs(c) for c in commits)
        )
    _stacked_parent_cache[p] = owned
    return owned


for p in changed:
    if os.path.basename(p) in ("BUILD.bazel", "BUILD.gn"):
        d = os.path.dirname(p).strip("/")
        if (
            d
            and d not in ("tools", "src", "sdk", "build")
            and not any(d == ex or d.startswith(ex + "/") for ex in EXCLUDED_GLOBAL_DIRS)
            and not _is_ancestor_test_wiring_only(d)
            and (f"{d}/BUILD.gn" in changed_set or _has_substantive_bazel_target_change(d))
            and not _owned_by_stacked_parent_commit(p)
            and d not in dirs
        ):
            dirs.append(d)

fx = None
for rel in (".jiri_root/bin/fx", "scripts/fx"):
    cand = os.path.join(workdir, rel)
    if os.path.isfile(cand):
        fx = cand
        break
if fx is None:
    print(json.dumps([{
        "source": "build_verification",
        "category": "build_setup",
        "severity": "ERROR",
        "message": f"Cannot find fx in {workdir} (.jiri_root/bin/fx or scripts/fx); the migration could not be built.",
    }], indent=2))
    sys.exit(0)

bazel_dirs = [d for d in dirs if os.path.isfile(os.path.join(workdir, d, "BUILD.bazel"))]
test_bazel_dirs_set = {d for d in bazel_dirs if _has_substantive_bazel_target_change(d)}
host_markers = re.compile(
    r"HOST_CONSTRAINTS|HOST_OS_CONSTRAINTS|_host_tool\b|with_host_unit_tests|"
    r"\bwith_unit_tests\s*=\s*\"(?:host|both)\"|\b(?:idk_)?ffx_(?:plugin|tool)\b|"
    r"\bhost_(?:go|rustc|py|cc|shac)_test\b|\bhost_test(?:_data)?\b|\bwrap_host_rust_test\("
)
host_dirs = []
for d in bazel_dirs:
    if d not in test_bazel_dirs_set:
        continue
    try:
        with open(os.path.join(workdir, d, "BUILD.bazel")) as f:
            if host_markers.search(f.read()):
                host_dirs.append(d)
    except OSError:
        pass

# Bazel tests exported to tests.json by GN `bazel_test_suite()` targets in the changed directories:
# {"host_tests": [labels], "target_tests": [labels]}. Host tests defined in the changed packages
# are run with `fx bazel test`; device tests (`fx_test`) cannot run under `bazel test` and are only
# built (bazel_build_fuchsia).
exported_tests = {"host_tests": [], "target_tests": []}
for d in dirs:
    try:
        with open(os.path.join(workdir, d, "BUILD.gn"), encoding="utf-8", errors="replace") as f:
            gn_text = re.sub(r"#[^\n]*", "", f.read())
    except OSError:
        continue
    for sm in re.finditer(r'\bbazel_test_suite\(\s*"[^"]+"\s*\)\s*\{', gn_text):
        depth, j = 0, sm.end() - 1
        while j < len(gn_text):
            depth += {"{": 1, "}": -1}.get(gn_text[j], 0)
            if depth == 0:
                break
            j += 1
        for lm in re.finditer(r"\b(host_tests|target_tests)\s*\+?=\s*\[([^\]]*)\]", gn_text[sm.end() : j]):
            for label in re.findall(r'"(//[^"]+)"', lm.group(2)):
                if label not in exported_tests[lm.group(1)]:
                    exported_tests[lm.group(1)].append(label)


# Split a Bazel label (`@//a/b:c`, `//a/b`, or `:c`) into `(pkg_dir, target_name)`.
def _split_bazel_label(raw_lbl, default_pkg=""):
    s = re.sub(r"^@@?//", "//", (raw_lbl or "").strip())
    if not s:
        return "", ""
    if s.startswith("//"):
        pkg_part, sep, tgt_part = s[2:].partition(":")
        pkg_d = pkg_part.strip("/")
        t_name = tgt_part.strip() if sep else (pkg_d.rsplit("/", 1)[-1] if pkg_d else "")
        return pkg_d, t_name
    if s.startswith(":"):
        return default_pkg, s[1:].strip()
    if "/" not in s and ":" not in s:
        return default_pkg, s
    return "", ""


_pkg_suites_cache = {}


# Parse `test_suite` targets in `<pkg_d>/BUILD.bazel`, mapping `suite_name` to either a list
# of member label strings (when `tests = [...]` is set) or `None` (implicit same-package suite).
def _bazel_pkg_test_suites(pkg_d):
    if pkg_d in _pkg_suites_cache:
        return _pkg_suites_cache[pkg_d]
    suites = {}
    bz_path = os.path.join(workdir, pkg_d, "BUILD.bazel")
    try:
        with open(bz_path, encoding="utf-8", errors="replace") as f:
            tree = ast.parse(f.read())
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            fn = (
                node.func.id
                if isinstance(node.func, ast.Name)
                else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
            )
            if fn != "test_suite":
                continue
            kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
            nm = kws.get("name")
            if not (isinstance(nm, ast.Constant) and isinstance(nm.value, str)):
                continue
            tests_kw = kws.get("tests")
            if tests_kw is None:
                suites[nm.value] = None
            else:
                member_list = [
                    c.value
                    for c in ast.walk(tests_kw)
                    if isinstance(c, ast.Constant) and isinstance(c.value, str)
                ]
                suites[nm.value] = member_list if member_list else None
    except Exception:
        pass
    _pkg_suites_cache[pkg_d] = suites
    return suites


# Expand an exported Bazel `host_tests` label to leaf host test targets in `test_dirs_set` so
# ancestor `test_suite` aggregators never execute unrelated sibling packages' tests on RBE.
def _expand_exported_host_test_label(raw_lbl, test_dirs_set, seen=None):
    if seen is None:
        seen = set()
    pkg_d, t_name = _split_bazel_label(raw_lbl)
    if not pkg_d or not t_name:
        return []
    canon_lbl = f"//{pkg_d}:{t_name}"
    if canon_lbl in seen:
        return []
    seen.add(canon_lbl)
    if (
        pkg_d not in test_dirs_set
        and pkg_d not in dirs
        and not any(td.startswith(pkg_d + "/") for td in test_dirs_set)
    ):
        return []
    suites = _bazel_pkg_test_suites(pkg_d)
    if t_name in suites:
        members = suites[t_name]
        if members is None:
            return [canon_lbl] if pkg_d in test_dirs_set else []
        expanded = []
        for m_raw in members:
            m_pkg, m_tgt = _split_bazel_label(m_raw, default_pkg=pkg_d)
            if not m_pkg or not m_tgt:
                continue
            for leaf in _expand_exported_host_test_label(
                f"//{m_pkg}:{m_tgt}", test_dirs_set, seen
            ):
                if leaf not in expanded:
                    expanded.append(leaf)
        return expanded
    return [canon_lbl] if pkg_d in test_dirs_set else []


# Collect Bazel host tests defined directly in the substantively changed packages' BUILD.bazel
# files (including tests wired into an ancestor BUILD.bazel `test_suite` rather than a local
# `bazel_test_suite`), expanding any exported `test_suite` targets so `bazel_test_host` never
# runs unrelated sibling packages' tests from an ancestor `test_suite`.
BAZEL_HOST_TEST_RULES = {
    "host_rustc_test",
    "wrap_host_rust_test",
    "host_go_test",
    "host_py_test",
    "host_test",
    "host_cc_test",
    "host_shac_test",
}
test_bazel_dirs_set = {d for d in bazel_dirs if _has_substantive_bazel_target_change(d)}
host_test_targets = []
for d in bazel_dirs:
    try:
        with open(os.path.join(workdir, d, "BUILD.bazel"), encoding="utf-8", errors="replace") as f:
            bz_src = f.read()
        bz_tree = ast.parse(bz_src)
    except Exception:
        continue
    for node in ast.walk(bz_tree):
        if not isinstance(node, ast.Call):
            continue
        fn = (
            node.func.id
            if isinstance(node.func, ast.Name)
            else (node.func.attr if isinstance(node.func, ast.Attribute) else "")
        )
        kws = {kw.arg: kw.value for kw in node.keywords if kw.arg}
        nm_node = kws.get("name")
        if not (isinstance(nm_node, ast.Constant) and isinstance(nm_node.value, str)):
            continue
        tname = nm_node.value
        if fn in BAZEL_HOST_TEST_RULES:
            lbl = f"//{d}:{tname}"
            if d in test_bazel_dirs_set and lbl not in host_test_targets:
                host_test_targets.append(lbl)
            if lbl not in exported_tests["host_tests"]:
                exported_tests["host_tests"].append(lbl)
        elif fn in ("fx_test", "fx_unittest_package"):
            lbl = f"//{d}:{tname}"
            if lbl not in exported_tests["target_tests"]:
                exported_tests["target_tests"].append(lbl)
        elif fn in ("rustc_library", "rustc_binary", "rustc_proc_macro"):
            w_host = kws.get("with_host_unit_tests")
            w_ut = kws.get("with_unit_tests")
            if (isinstance(w_host, ast.Constant) and w_host.value is True) or (
                isinstance(w_ut, ast.Constant) and w_ut.value in ("host", "both")
            ):
                lbl = f"//{d}:{tname}_test"
                if d in test_bazel_dirs_set and lbl not in host_test_targets:
                    host_test_targets.append(lbl)
                if lbl not in exported_tests["host_tests"]:
                    exported_tests["host_tests"].append(lbl)
for lbl in exported_tests["host_tests"]:
    for leaf_lbl in _expand_exported_host_test_label(lbl, test_bazel_dirs_set):
        if leaf_lbl not in host_test_targets:
            host_test_targets.append(leaf_lbl)

LINKED_TEMPLATES = {"executable", "test", "rustc_binary", "rustc_test", "loadable_module", "shared_library", "rustc_cdylib", "fuchsia_driver"}
LIBRARY_TEMPLATES = {"rustc_library", "rustc_macro", "rustc_staticlib", "static_library", "source_set"}
GN_TARGET = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
GN_DEP_LIST = re.compile(r"(?<![\w.])(?:public_deps|deps|non_rust_deps|link_deps|test_deps|data_deps)\s*\+?=\s*\[([^\]]*)\]")
GN_DEP_VAR = re.compile(r"(?<![\w.])(?:public_deps|deps|non_rust_deps|link_deps|test_deps|data_deps)\s*\+?=\s*([A-Za-z_][^\n]*)")
LIST_VAR = re.compile(r"(?<![\w.])([A-Za-z_]\w*)\s*\+?=\s*\[([^\]]*)\]")
UNIT_TESTS = re.compile(r"(?<![\w.])with_unit_tests\s*=\s*true")


def read(rel):
    try:
        with open(os.path.join(workdir, rel), encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return None


def gn_code(text):
    """Blanks out GN comments without moving offsets, so strings and brackets can be scanned."""
    out, i, n, in_str = [], 0, len(text), False
    while i < n:
        c = text[i]
        if in_str:
            if c == "\\":
                out.append(text[i : i + 2])
                i += 2
                continue
            in_str = c != '"'
        elif c == '"':
            in_str = True
        elif c == "#":
            j = text.find("\n", i)
            j = n if j < 0 else j
            out.append(" " * (j - i))
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out)


def closing(code, i):
    """Index of the `}` closing the `{` at code[i]."""
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


def gn_blocks(text):
    """{name: [template, body]} for every `template("name") { ... }` block of a BUILD.gn."""
    code = gn_code(text)
    blocks = {}
    for m in GN_TARGET.finditer(code):
        if m.group(1) in ("template", "declare_args", "foreach", "forward_variables_from"):
            continue
        brace = m.end() - 1
        blocks.setdefault(m.group(2), [m.group(1), ""])[1] += code[brace + 1 : closing(code, brace)]
    return blocks


def label_dir(raw, pkg):
    """Package directory of a GN label written in package pkg; None for labels built from variables."""
    raw = raw.strip()
    if not raw or "$" in raw or raw.startswith("@"):
        return None
    path = re.sub(r"\(.*\)$", "", raw).split(":", 1)[0]
    if path.startswith("//"):
        return path[2:].strip("/")
    return os.path.normpath(os.path.join(pkg, path)).strip("/") if path else pkg


def rdep_build_files():
    """BUILD.gn files outside the changed directories that mention one of them."""
    patterns = [a for d in dirs for a in ("-e", f"//{d}")]
    rg = shutil.which("rg", path=os.environ.get("PATH", "") + os.pathsep + os.path.expanduser("~/.cargo/bin"))
    cmd = (
        [rg, "-l", "--fixed-strings", "-g", "BUILD.gn"] + patterns + ["."]
        if rg
        else ["git", "grep", "-l", "--fixed-strings"] + patterns + ["--", "BUILD.gn", "*/BUILD.gn"]
    )
    try:
        out = subprocess.run(cmd, cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, timeout=600).stdout
    except (OSError, subprocess.SubprocessError):
        return []
    files = {os.path.normpath(l.strip()) for l in out.splitlines() if l.strip()}
    return sorted(f for f in files if os.path.dirname(f) not in dirs and not f.startswith("out/"))


def load_toolchains():
    try:
        with open(os.path.join(workdir, ".fx-build-dir")) as f:
            build_dir = f.read().strip()
        with open(os.path.join(workdir, build_dir, "ninja_outputs.json")) as f:
            known = json.load(f)
    except (OSError, ValueError) as e:
        return None, str(e)
    toolchains = {}
    for key, outs in sorted(known.items()):
        if isinstance(outs, list) and any(isinstance(o, str) and o.strip() for o in outs):
            toolchains.setdefault(key.split("(", 1)[0], []).append(key)
    return toolchains, None


# Collect GN target labels in changed BUILD.gn files that are absent from the default-toolchain
# entries of ninja_outputs.json (including host-only targets such as ffx_plugin/ffx_tool).
def unconfigured_own_gn_labels(toolchains):
    if toolchains is None:
        return []
    unconfigured = []
    for d in dirs:
        blocks = gn_blocks(read(f"{d}/BUILD.gn") or "")
        for name, (tmpl, _) in sorted(blocks.items()):
            if tmpl == "config":
                continue
            lbl = f"//{d}:{name}"
            if lbl not in toolchains.get(lbl, []) and lbl not in unconfigured:
                unconfigured.append(lbl)
    return unconfigured


# Build an INFO note warning against passing unconfigured or host-only GN labels to `fx build`
# in `Test:` footers, and clarifying that `fx bazel build` supersedes generic `fx build` defaults.
def unconfigured_own_gn_note(unconfigured):
    if not unconfigured:
        return None
    shown = " ".join(unconfigured)
    return (
        f"Not in the default-toolchain GN graph (<build dir>/ninja_outputs.json): {shown}. "
        f"Do NOT pass unqualified `{shown}` to `fx build` in `CoderReport.test_footers` or "
        "`Test:` commit footers (Planter's `test_verifier` runs `fx gn desc` in the default "
        "toolchain and fails with `missing_gn_test_target` / `Unknown GN label` on host-only "
        "or unconfigured targets); verify migrated Bazel targets with "
        "`fx bazel build --config=fuchsia_platform @//<dir>:<target>` (or `--config=host` for "
        "host targets) instead. Using `fx bazel build` here supersedes generic "
        "`fx build //<dir>:<target>` task-prompt instructions and is NOT a `machinery_conflict` "
        "(never report it in `disputed_findings`)."
    )


# Check whether the `test` host tool required by `fx test --dry` exists in the build directory.
def missing_fx_test_tool():
    build_dir = (read(".fx-build-dir") or "").strip()
    if not build_dir:
        return None
    try:
        with open(os.path.join(workdir, build_dir, "tool_paths.json")) as f:
            tp_data = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(tp_data, list):
        return None
    for entry in tp_data:
        if not isinstance(entry, dict) or entry.get("name") != "test":
            continue
        rel_path = (entry.get("path") or "").strip()
        lbl = (entry.get("label") or "//scripts/fxtest/python:install").strip()
        if rel_path and not os.path.isfile(os.path.join(workdir, build_dir, rel_path)):
            return f"{build_dir}/{rel_path}", lbl
    return None


# Collect unmigrated pre-sentinel test targets in `<d>/BUILD.gn` that are explicitly
# listed in `assert_no_deps` of an ancestor `BUILD.gn` or reverse-dependency `BUILD.gn`.
def excluded_presen_test_labels(d, rdep_files):
    gn_src = read(f"{d}/BUILD.gn") or ""
    if "BAZEL2GN SENTINEL" not in gn_src and "#LOCAL_BAZEL_BUILD_SENTINEL" not in gn_src:
        return set()
    pre_sen = re.split(
        r"(?m)^[ \t]*(?:##\s*BAZEL2GN SENTINEL|#LOCAL_BAZEL_BUILD_SENTINEL)",
        gn_src,
        maxsplit=1,
    )[0]
    pre_blocks = gn_blocks(pre_sen)
    if not pre_blocks:
        return set()
    cand_files = list(rdep_files or [])
    parts = [p for p in d.split("/") if p]
    for i in range(1, len(parts)):
        anc_rel = "/".join(parts[:i]) + "/BUILD.gn"
        if anc_rel not in cand_files and os.path.isfile(os.path.join(workdir, anc_rel)):
            cand_files.append(anc_rel)
    asserted = set()
    assert_re = re.compile(r"(?<![\w.])assert_no_deps\s*\+?=\s*\[([^\]]*)\]")
    for rel in cand_files:
        pkg = os.path.dirname(rel)
        txt = gn_code(read(rel) or "")
        for m in assert_re.finditer(txt):
            for raw_lbl in re.findall(r'"([^"]+)"', m.group(1)):
                clean_lbl = re.sub(r"\(.*\)$", "", raw_lbl.strip())
                if label_dir(clean_lbl, pkg) != d:
                    continue
                tname = clean_lbl.split(":", 1)[1] if ":" in clean_lbl else d.rsplit("/", 1)[-1]
                if tname in pre_blocks or (tname.endswith("_test") and tname[:-5] in pre_blocks):
                    asserted.add(f"//{d}:{tname}")
    if f"//{d}:tests" in asserted and "tests" in pre_blocks:
        _, tests_body = pre_blocks["tests"]
        for raw_dep in re.findall(r'"([^"]+)"', tests_body):
            clean_dep = re.sub(r"\(.*\)$", "", raw_dep.strip())
            if label_dir(clean_dep, d) == d:
                dep_name = clean_dep.split(":", 1)[1] if ":" in clean_dep else d.rsplit("/", 1)[-1]
                if dep_name in pre_blocks or (dep_name.endswith("_test") and dep_name[:-5] in pre_blocks):
                    asserted.add(f"//{d}:{dep_name}")
    return asserted


def dependent_labels():
    """(labels, notes) for gn_build_dependents: the changed packages' `:tests` groups and test/binary/library targets,
    then GN targets of other packages that depend on a changed package (tests and binaries, `<name>_test` of
    `with_unit_tests` Rust targets, then libraries), as keys of <build dir>/ninja_outputs.json."""
    toolchains, err = load_toolchains()
    if toolchains is None:
        return [], [f"Cannot read the GN target list (<build dir>/ninja_outputs.json): {err}. GN dependents were not built."]
    own, linked, unit_tests, libraries = [], [], [], []
    unconfigured_own_tests = unconfigured_own_gn_labels(toolchains)
    rdep_files = rdep_build_files()
    excluded_presen = set()
    for d in dirs:
        excl_d = excluded_presen_test_labels(d, rdep_files)
        excluded_presen |= excl_d
        blocks = gn_blocks(read(f"{d}/BUILD.gn") or "")
        if "tests" in blocks and f"//{d}:tests" not in excl_d:
            own.append(f"//{d}:tests")
        for name, (tmpl, body) in sorted(blocks.items()):
            lbl = f"//{d}:{name}"
            if lbl in excl_d:
                continue
            if (
                tmpl in LINKED_TEMPLATES
                or tmpl in LIBRARY_TEMPLATES
                or tmpl in ("bazel_fuchsia_package", "bazel_input_file")
            ):
                own.append(lbl)
            if tmpl.startswith("rustc_") and UNIT_TESTS.search(body) and f"{lbl}_test" not in excl_d:
                own.append(f"{lbl}_test")
    for rel in rdep_files:
        pkg = os.path.dirname(rel)
        text = read(rel) or ""

        def names_changed(items):
            return bool({label_dir(s, pkg) for s in re.findall(r'"([^"]*)"', items)} & set(dirs))

        # List variables (e.g. `_core_deps = [ ... ]`) naming a changed package, used as `deps = _core_deps`.
        dep_vars = {
            m.group(1)
            for m in LIST_VAR.finditer(gn_code(text))
            if m.group(1) not in ("public_deps", "deps", "non_rust_deps", "link_deps", "test_deps", "data_deps", "visibility")
            and names_changed(m.group(2))
        }
        for name, (tmpl, body) in sorted(gn_blocks(text).items()):
            if not any(names_changed(m.group(1)) for m in GN_DEP_LIST.finditer(body)) and not any(
                re.search(r"(?<![\w.])(%s)\b" % "|".join(map(re.escape, dep_vars)), m.group(1))
                for m in GN_DEP_VAR.finditer(body)
                if dep_vars
            ):
                continue
            if tmpl in LINKED_TEMPLATES:
                linked.append(f"//{pkg}:{name}")
            if tmpl.startswith("rustc_") and UNIT_TESTS.search(body):
                unit_tests.append(f"//{pkg}:{name}_test")
            if tmpl in LIBRARY_TEMPLATES:
                libraries.append(f"//{pkg}:{name}")
    labels = []
    missing_tool = missing_fx_test_tool()
    if missing_tool is not None and (
        exported_tests["host_tests"]
        or exported_tests["target_tests"]
        or any(l.endswith(":tests") for l in own)
    ):
        labels.append(missing_tool[1])
    for label in dict.fromkeys(own + linked + unit_tests + libraries):
        keys = toolchains.get(label, [])
        key = label if label in keys else next((k for k in keys if "host" in k), keys[0] if keys else None)
        if key and key not in labels:
            labels.append(key)
    notes = []
    if excluded_presen:
        shown_excl = " ".join(sorted(excluded_presen))
        notes.append(
            f"Skipped unmigrated pre-sentinel test target(s) excluded from ancestor "
            f'group("tests") via assert_no_deps: {shown_excl}.'
        )
    unconf_note = unconfigured_own_gn_note(unconfigured_own_tests)
    if unconf_note:
        notes.append(unconf_note)
    if len(labels) > dependent_limit:
        notes.append(
            f"gn_build_dependents builds only the first {dependent_limit} of {len(labels)} labels "
            f"(PLANTER_DEPENDENT_BUILD_LIMIT); not built: {' '.join(labels[dependent_limit:])}"
        )
        labels = labels[:dependent_limit]
    return labels, notes


def bazel_tests_export_notes():
    """INFO notes on Bazel tests defined or exported by the changed directories but
    missing from <build dir>/tests.json."""
    build_dir = (read(".fx-build-dir") or "").strip()
    try:
        with open(os.path.join(workdir, build_dir, "tests.json")) as f:
            entries = json.load(f)
    except (OSError, ValueError) as e:
        return [f"Cannot read <build dir>/tests.json ({e}); the export of Bazel tests was not verified."]

    def norm(label):
        return re.sub(r"^@@?//", "//", label or "").split("(", 1)[0]

    bazel_entries = set()
    for e in entries if isinstance(entries, list) else []:
        t = e.get("test", {}) if isinstance(e, dict) else {}
        for k in ("label", "source_label", "package_label"):
            lbl = norm(t.get(k))
            if lbl:
                bazel_entries.add(lbl)
    missing = []
    for label in exported_tests["target_tests"] + exported_tests["host_tests"]:
        if label not in bazel_entries:
            missing.append(label)
    if not missing:
        return []
    missing_str = " ".join(missing)
    return [
        f"Not in <build dir>/tests.json after `fx build`: {missing_str} "
        "(normal when `args.gn` is configured without `--with-test //<dir>:tests` or "
        "only includes leaf `:tests` groups and not the ancestor `bazel_test_suite`). "
        f"Do NOT pass `{missing_str}` to `fx test` in `CoderReport.test_footers` or "
        "commit `Test:` footers (`fx test --dry` will fail with `missing_fx_test_target` "
        "because `<build dir>/tests.json` does not contain that label); use "
        "`fx bazel build --config=fuchsia_platform @//<dir>:<test>` (or "
        "`fx bazel test --config=host @//<dir>:<test>` for host tests) instead. "
        "Using `fx bazel test|build` when `<build dir>/tests.json` omits the target "
        "supersedes `fx test` instructions and is NOT a `machinery_conflict` "
        "(never report it in `disputed_findings`)."
    ]


FIDL_MACROS = {"fidl_library", "_fidl_library", "fidl_ir"}
STRUCTURED_CONFIG_FIDL_MACROS = {
    "fuchsia_structured_config_rust_lib",
    "fuchsia_structured_config_cpp_elf_lib",
    "fuchsia_structured_config_cpp_driver_lib",
}
VALIDATE_JSON_MACROS = {"validate_json", "validate_json5", "_validate_json_action"}
FDOMAIN_UNSUPPORTED_HANDLE_RE = re.compile(
    r"\bzx\.Handle\s*:\s*(?:LOG|DEBUGLOG|INTERRUPT|MSI|IOMMU|BTI|PMT|PAGER|PCI_DEVICE|PROFILE|SUSPEND_TOKEN|VCPU)\b"
)


def _dir_has_fdomain_unsupported_handles(d):
    abs_d = os.path.join(workdir, d)
    try:
        names = os.listdir(abs_d)
    except OSError:
        return False
    for fn in sorted(names):
        if fn.endswith(".fidl"):
            content = read(f"{d}/{fn}") or ""
            if FDOMAIN_UNSUPPORTED_HANDLE_RE.search(content):
                return True
    return False


# Exclude `_validate_json_action` targets (from `fidl_library`, structured config macros, and
# `validate_json`) from top-level `:all` builds so `assert_no_deps_aspect` does not crash.
def validate_json_exclusions(d):
    """Returns `-//<d>:<target>` exclusions for `_validate_json_action` targets in `d/BUILD.bazel`
    (including `<name>_fidl_internal_validate_ir_json` expanded by `fuchsia_structured_config_*`
    macros, and `<name>_rust_fdomain{,_flex}` when `.fidl` sources use `zx.Handle` subtypes missing
    from `fdomain_client`). `//build/bazel/aspects:assert_no_deps.bzl` iterates
    `getattr(ctx.rule.attr, "data", [])`, which crashes with `Error: type 'Target' is not iterable`
    when `:all` applies `assert_no_deps_aspect` at the top level to `_validate_json_action`
    (`data = attr.label(...)`). Excluding `<name>_validate_ir_json` (and
    `<name>_fidl_internal_validate_ir_json` from `fuchsia_structured_config_*_lib`) from top-level
    `:all` still runs IR JSON validation via the FIDL target's `_validation` output group."""
    text = read(f"{d}/BUILD.bazel")
    if not text:
        return []
    out = []
    seen = set()
    skip_fdomain = _dir_has_fdomain_unsupported_handles(d)

    def _add(target):
        lbl = f"-//{d}:{target}"
        if lbl not in seen:
            seen.add(lbl)
            out.append(lbl)

    def _add_fidl(tname, fn):
        if fn in STRUCTURED_CONFIG_FIDL_MACROS:
            _add(f"{tname}_fidl_internal_validate_ir_json")
            return
        _add(f"{tname}_validate_ir_json" if fn in FIDL_MACROS else tname)
        if fn in {"fidl_library", "_fidl_library"} and skip_fdomain:
            _add(f"{tname}_rust_fdomain")
            _add(f"{tname}_rust_fdomain_flex")

    try:
        tree = ast.parse(text)
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            fn = node.func.id if isinstance(node.func, ast.Name) else (
                node.func.attr if isinstance(node.func, ast.Attribute) else None
            )
            if (
                fn not in FIDL_MACROS
                and fn not in STRUCTURED_CONFIG_FIDL_MACROS
                and fn not in VALIDATE_JSON_MACROS
            ):
                continue
            for kw in node.keywords:
                if kw.arg == "name" and isinstance(kw.value, ast.Constant) and isinstance(kw.value.value, str):
                    _add_fidl(kw.value.value, fn)
    except SyntaxError:
        for m in re.finditer(
            r"\b(fidl_library|_fidl_library|fidl_ir|fuchsia_structured_config_(?:rust|cpp_elf|cpp_driver)_lib|validate_json5?|_validate_json_action)\s*\(([^)]*)\)",
            gn_code(text),
            re.S,
        ):
            fn, args_body = m.group(1), m.group(2)
            nm = re.search(r'\bname\s*=\s*"([^"]+)"', args_body)
            if nm:
                _add_fidl(nm.group(1), fn)
    return out


def has_non_suite_bazel_targets(d):
    """True when `d/BUILD.bazel` defines at least one rule/macro target other than `test_suite`,
    `package`, `exports_files`, or `licenses` (so `bazel_build_fuchsia` does not build
    `test_suite`-only ancestor aggregator packages under `--config=fuchsia_platform`)."""
    text = read(f"{d}/BUILD.bazel")
    if not text:
        return False
    non_build_calls = {"package", "exports_files", "licenses", "load", "test_suite"}
    try:
        tree = ast.parse(text)
        for node in tree.body:
            if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
                fn = node.value.func.id if isinstance(node.value.func, ast.Name) else (
                    node.value.func.attr if isinstance(node.value.func, ast.Attribute) else None
                )
                if fn and fn not in non_build_calls:
                    return True
        return False
    except SyntaxError:
        calls = set(re.findall(r"^\s*([A-Za-z_]\w*)\s*\(", gn_code(text), re.M))
        return bool(calls - non_build_calls)


def bazel_target_args(pkg_dirs):
    pos = [f"//{d}:all" for d in pkg_dirs]
    neg = [lbl for d in pkg_dirs for lbl in validate_json_exclusions(d)]
    return (["--"] + pos + neg) if neg else pos


# Pin the @gn_targets repository the way `fx build --fuchsia_platform` / `--host` do.
# Prefer `<build_dir>/build/bazel/tests_json.gn_targets` whenever generated by `fx gen` so
# both target and host Bazel builds/tests can resolve exported `@gn_targets` inputs, falling
# back to `empty_gn_targets` for host builds when `tests_json.gn_targets` is absent.
def gn_targets_override(config):
    build_dir = (read(".fx-build-dir") or "").strip()
    if build_dir:
        tests_json_repo = os.path.join(
            workdir, build_dir, "build", "bazel", "tests_json.gn_targets"
        )
        if os.path.isdir(tests_json_repo):
            return [f"--override_repository=gn_targets+={tests_json_repo}"]
    if config == "host":
        empty_repo = os.path.join(
            workdir, "build", "bazel", "local_repositories", "empty_gn_targets"
        )
        if os.path.isdir(empty_repo):
            return [f"--override_repository=gn_targets+={empty_repo}"]
    return []


def fmt_cmd(cmd):
    return shlex.join(["fx"] + cmd[1:])


steps = [
    ("gn_build", [fx, "build"]),
    ("gn_build_dependents", None),  # Labels are computed once gn_build has regenerated the GN graph.
]
# `fx lint` without file arguments finds and checks all modified files itself;
# `--fail-level=warning` makes `shac check` fail on warnings as well as errors.
steps.append(("shac_check", [fx, "lint", "--fail-level=warning"]))
fuchsia_bazel_dirs = [
    d for d in bazel_dirs if d in test_bazel_dirs_set and has_non_suite_bazel_targets(d)
]


# Build Bazel command lines dynamically so `gn_targets_override(...)` reflects any
# `<build_dir>/build/bazel/tests_json.gn_targets` repository created during `gn_build`.
def build_bazel_step_cmd(step_name):
    if step_name == "bazel_build_fuchsia":
        return (
            [fx, "bazel", "build", "--config=fuchsia_platform", "--config=remote", "--noexpand_test_suites"]
            + gn_targets_override("fuchsia_platform")
            + bazel_target_args(fuchsia_bazel_dirs)
        )
    if step_name == "bazel_build_host":
        return (
            [fx, "bazel", "build", "--config=host", "--config=remote"]
            + gn_targets_override("host")
            + bazel_target_args(host_dirs)
        )
    if step_name == "bazel_test_host":
        return (
            [fx, "bazel", "test", "--config=host", "--config=remote"]
            + gn_targets_override("host")
            + host_test_targets
        )
    return None

if fuchsia_bazel_dirs:
    steps.append(("bazel_build_fuchsia", build_bazel_step_cmd("bazel_build_fuchsia")))
if host_dirs:
    steps.append(("bazel_build_host", build_bazel_step_cmd("bazel_build_host")))
if host_test_targets:
    steps.append(("bazel_test_host", build_bazel_step_cmd("bazel_test_host")))
if exported_tests["host_tests"] or exported_tests["target_tests"]:
    steps.append(("bazel_tests_exported", None))  # Checks <build dir>/tests.json after gn_build.

ERROR_LINE = re.compile(
    r"(^ERROR:|^FAILED:|\berror(\[E\d+\])?:|^error\b|: error\b|/(?:error|warning)\]|^shac: fail:|\(error in\b|"
    r"\bundefined reference\b|no such (target|package)|is not visible|Unable to load|not declared|"
    r"out of sync|drift|differs|--- a/|\+\+\+ b/)",
    re.I,
)
# `<path>:<line>` or `<path>(<line>)`, where <path> starts a token (start of line, after whitespace, a
# quote, or an opening bracket) so log text that merely contains a dotted word is not taken as a location.
LOCATION = re.compile(
    r"(?:-->\s*)?(?:^|(?<=[\s'\"`(\[<>]))"
    r"((?:\.\./\.\./)?[\w./+-]+\.(?:rs|cc|h|c|gn|gni|bazel|bzl|cml|fidl|json5?|md|py|go|star|sh|txt))"
    r"(?::|\()(\d+)\)?",
    re.M,
)
BUILD_FILE_LOCATION = re.compile(r"((?:\.\./\.\./)?[\w./+-]*(?:BUILD\.gn|BUILD\.bazel|\.gni|\.bzl)):(\d+)")
# Shac findings: `[<check>/error] <path>(<line>): ...` or `[<check>/warning] <path>: ...`,
# and commit findings `[<check>/warning] Commit <sha>(<line>): ...`.
SHAC_FILE_LOCATION = re.compile(r"\[[\w_-]+/(?:error|warning)\]\s+(?!Commit\s+[0-9a-f]{7,40}(?:\(\d+\))?:)([\w./+-]+)(?:\((\d+)\))?:")
SHAC_COMMIT_LOCATION = re.compile(r"\[[\w_-]+/(?:error|warning)\]\s+Commit\s+([0-9a-f]{7,40})(?:\((\d+)\))?:")
SHAC_FINDING = re.compile(
    r"^\[([\w_-]+)/(notice|warning|error)\]\s+"
    r"(?:Commit\s+([0-9a-f]{7,40})|([\w./+-]+))(?:\((\d+)\))?:\s*([^\n]*)",
    re.M,
)
_FX_LINT_CALLED_PROCESS_TRACEBACK = re.compile(
    r"\n*Traceback \(most recent call last\):\n"
    r'  File "[^"]*tools/devshell/lint\.py"[\s\S]*?'
    r"^subprocess\.CalledProcessError:[^\n]*\n?",
    re.M,
)


def summarize(output, location=LOCATION):
    lines = output.splitlines()
    picked = []
    for i, line in enumerate(lines):
        if ERROR_LINE.search(line):
            for j in range(max(0, i - 1), min(len(lines), i + 4)):
                if not picked or picked[-1] < j:
                    picked.append(j)
    raw_text = "\n".join(lines[j] for j in picked) if picked else "\n".join(lines[-40:])
    text = raw_text[:6000] + "\n... (truncated)" if len(raw_text) > 6000 else raw_text
    file, line = "", 0
    if location is SHAC_FILE_LOCATION:
        cur_head = (git_lines(["rev-parse", "HEAD"]) or [""])[0]
        cur_base = (git_lines(["rev-parse", "--verify", f"{change_base}^{{commit}}"]) or [""])[0]
        is_task_head = bool(
            cur_head
            and cur_base
            and cur_head != cur_base
            and not worktree_dirty_vs_head
            and (not uploaded_commit_env or cur_head != uploaded_commit_env)
        )
        first_shac = None
        for m in SHAC_FINDING.finditer(raw_text):
            if m.group(2) not in ("warning", "error"):
                continue
            c_sha, f_raw, ln_str, msg_text = m.group(3), m.group(4), m.group(5), m.group(6) or ""
            if c_sha:
                is_rel = is_task_head and cur_head.startswith(c_sha)
                cand_f, cand_ln = ("COMMIT_MSG" if is_rel else ""), int(ln_str or 0)
            else:
                cand_f = f_raw.replace("../../", "", 1)
                if cand_f.startswith(workdir + "/"):
                    cand_f = cand_f[len(workdir) + 1:]
                cand_ln = int(ln_str or 0)
                norm_f = os.path.normpath(cand_f)
                in_chg = _shac_finding_in_change(norm_f, msg_text)
                is_rel = in_chg and not (
                    m.group(2) == "warning" and _is_unchanged_preexisting_warning(norm_f, cand_ln)
                )
            if first_shac is None:
                first_shac = (cand_f, cand_ln)
            if is_rel:
                if m.end() > 6000:
                    snippet_start = max(0, raw_text.rfind("\n", 0, m.start()) + 1)
                    text = text + "\n" + raw_text[snippet_start : snippet_start + 1000]
                return text, cand_f, cand_ln
        if first_shac is not None:
            return text, first_shac[0], first_shac[1]
    m = location.search(text) or SHAC_FILE_LOCATION.search(text)
    if m:
        file = m.group(1).replace("../../", "", 1)
        if file.startswith(workdir + "/"):
            file = file[len(workdir) + 1:]
        line = int(m.group(2) or 0)
    else:
        cm = SHAC_COMMIT_LOCATION.search(text)
        if cm:
            file, line = "COMMIT_MSG", int(cm.group(2) or 0)
    return text, file, line


REMEDIATION = {
    "gn_build": "Fix the BUILD.bazel/BUILD.gn so the GN build passes (renamed/removed labels that dependents still use, "
                "missing deps, attribute drift), then re-run `fx bazel2gn -d <dir>` and `fx build`. Never edit sources to make it pass.",
    "gn_build_dependents": "A GN target that the default `fx build` does not build failed: a test or binary of a changed "
                           "package, or a GN dependent in another package. Undefined symbols at link time (e.g. `operator "
                           "new`/`operator delete` from C++ code linked into a Rust test) or missing configs mean the "
                           "migration changed a GN dependency label or edge kind: a GN-only wrapper (e.g. a `*_static` "
                           "target carrying link settings) replaced by the target it wraps, or a `public_deps` edge turned "
                           "into `deps`. Restore the original labels and edge kinds in the migrated packages (see the "
                           "`gn_dep_parity` findings and \"Keep Every GN Dependency Label and Edge Kind\" in the coder "
                           "instructions), re-run `fx bazel2gn -d <dir>` and `fx build <failing label>`. Never edit sources "
                           "or the dependents' BUILD files to make it pass.",
    "shac_check": "Fix the `fx lint` (`shac check`) findings above: if BUILD.gn is out of sync with BUILD.bazel (`verify_bazel2gn`), "
                  "run `fx bazel2gn -d <dir>` for every dual-build directory you touched (never hand-edit below the "
                  "BAZEL2GN SENTINEL); if a build or manifest file is unformatted, format it (`fx format-code`); if "
                  "`third_party_readme_update` failed on build-only `third_party/` edits, add "
                  "`Skip-Readme-Check: No version change, Bazel build files only.` to the contiguous commit footer "
                  "block (do not edit `README.fuchsia`); then re-run this check.",
    "bazel_build_fuchsia": (
        "Fix BUILD.bazel so the `bazel_build_fuchsia` command passes. Never edit `.rs`/`.cc`/`.h` "
        "source files; if an unreferenced unit test from a GN template fails due to pre-existing "
        "bitrot, omit that unit test target in BUILD.bazel with a `# TODO` comment and note it "
        "in the commit message. Do not sweep cross-area or general-purpose dependencies into "
        "this CL (see //build/beads/references/migration/scoping_changelists.md)."
    ),
    "bazel_build_host": (
        "Fix BUILD.bazel so the `bazel_build_host` command passes. Never edit `.rs`/`.cc`/`.h` "
        "source files; if an unreferenced unit test from a GN `with_unit_tests = true` target "
        "(whose package `group(\"tests\")` was never wired into any parent/ancestor `BUILD.gn`) "
        "fails to compile due to pre-existing bitrot, omit that unit test target in BUILD.bazel "
        "(and remove any unreachable `bazel_test_suite` / `group(\"tests\")` in BUILD.gn) with "
        "a `# TODO` comment and note it in the commit message. If a non-test target has "
        "unconditional Fuchsia-only dependencies and cannot build on host, add "
        "`target_compatible_with = [\"@platforms//os:fuchsia\"]` with a `# Needs reviewer "
        "attention (target_compatible_with):` comment above the target and explain it in the "
        "commit message body."
    ),
    "bazel_test_host": (
        "A Bazel host test exported by a GN `bazel_test_suite(host_tests = ...)` fails, so it "
        "would fail on infra too. Fix its BUILD.bazel attributes (deps, data/test_data, "
        "test_args, lint/rustc flags) to match what GN ran on host. If the unit test was never "
        "wired into any parent/ancestor `BUILD.gn` `tests` group and fails to compile due to "
        "pre-existing bitrot, omit the unit test target with a `# TODO` comment (and remove its "
        "unreachable `bazel_test_suite` / `group(\"tests\")`) and note it in the commit message. "
        "If GN only ran it on Fuchsia, package it as an `fx_test` / `fx_unittest_package` instead."
    ),
}

findings = []
head_sha = (git_lines(["rev-parse", "HEAD"]) or [""])[0]
base_sha = (git_lines(["rev-parse", "--verify", f"{change_base}^{{commit}}"]) or [""])[0]
head_is_upstream = _is_upstream_commit(head_sha)
uploaded_commit_env = os.environ.get("PLANTER_UPLOADED_COMMIT", "").strip()
worktree_dirty_vs_head = bool(
    git_lines(["diff", "--name-only", "HEAD", "--"])
    or [u for u in git_lines(["ls-files", "--others", "--exclude-standard"]) if any(u == d or u.startswith(d + "/") for d in dirs)]
)
head_subj = (git_lines(["log", "-1", "--format=%s", "HEAD"]) or [""])[0]
has_local_commit = bool(
    changed
    and not head_is_upstream
    and ((head_sha and base_sha and head_sha != base_sha) or "[bazel_migration]" in head_subj)
)
if has_local_commit and not worktree_dirty_vs_head:
    toolchains_map, _ = load_toolchains()
    tests_json_targets = None
    build_dir_rel = (read(".fx-build-dir") or "").strip()
    if build_dir_rel:
        try:
            with open(os.path.join(workdir, build_dir_rel, "tests.json")) as f:
                tj_data = json.load(f)
            if isinstance(tj_data, list):
                tests_json_targets = set()
                for entry in tj_data:
                    t_obj = entry.get("test", {}) if isinstance(entry, dict) else {}
                    for k in ("label", "source_label", "name", "package_url"):
                        val = t_obj.get(k)
                        if isinstance(val, str) and val.strip():
                            tests_json_targets.add(re.sub(r"^@@?//", "//", val.strip()).split("(", 1)[0])
        except (OSError, ValueError):
            tests_json_targets = None
    commit_msg = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
    # Build the `test` host tool if a commit `Test:` footer runs `fx test` and a cached
    # or target-scoped build left `<build dir>/host_*/test` unbuilt on disk.
    if (
        not skip_build
        and not dry_run
        and re.search(r"(?m)^\s*Test\s*:.*\bfx\s+(?:-t\s+\S+\s+)?test\b", commit_msg)
    ):
        missing_tool = missing_fx_test_tool()
        if missing_tool is not None and workspace_write_probe_error() is None:
            try:
                subprocess.run(
                    [fx, "build", missing_tool[1]],
                    cwd=workdir,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    timeout=timeout,
                )
            except subprocess.TimeoutExpired:
                pass
    test_footer_lines = []
    for m_line in commit_msg.splitlines():
        tm = re.match(r"^\s*Test\s*:\s*(.+)$", m_line, re.I)
        if not tm:
            continue
        test_footer_lines.append(m_line.strip())
        test_cmd = tm.group(1).strip()
        if re.match(r"^(?:fx|ffx|bazel)\b", test_cmd):
            syntax_proc = subprocess.run(
                ["bash", "-n", "-c", test_cmd],
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            if syntax_proc.returncode != 0:
                err_detail = " ".join(syntax_proc.stdout.strip().splitlines()) or f"exit status {syntax_proc.returncode}"
                findings.append({
                    "source": "build_verification",
                    "category": "malformed_test_footer_command",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` has a bash syntax error ({err_detail}). "
                        "Planter's test_verifier runs `Test:` commands via `bash -c`, so unquoted parentheses "
                        "in GN toolchain labels like `//pkg:target(//build/toolchain:...)` fail."
                    ),
                    "remediation": (
                        "Quote any GN label containing toolchain parentheses in the commit `Test:` footer and "
                        "`CoderReport.test_footers` (e.g. `fx build '//pkg:target(//build/toolchain:...)'`), or copy "
                        "the exact quoted `fx build` command from `PLANTER_BUILD_DRY_RUN=1 run_checks.sh --only "
                        "build_verification` (never its `fx bazel build ... :all` command: commit_message_format "
                        "rejects `:all` and labels without `@` in `Test:` footers)."
                    ),
                })
        if "fx bazel build" in m_line:
            missing_excls = []
            for d_all in re.findall(r"//([^\s:;\"'`]+):all\b", m_line):
                for excl in validate_json_exclusions(d_all.strip("/")):
                    if excl not in m_line and excl not in missing_excls:
                        missing_excls.append(excl)
            if missing_excls:
                findings.append({
                    "source": "build_verification",
                    "category": "unexcluded_validate_json_in_test_footer",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` runs `fx bazel build` with `:all` on a package "
                        f"defining `fidl_library`/`validate_json` without excluding `{' '.join(missing_excls)}`. "
                        "`assert_no_deps_aspect` crashes with `Error: type 'Target' is not iterable` when `:all` "
                        "applies it at the top level to `_validate_json_action`."
                    ),
                    "remediation": (
                        "Do not use `:all` in the commit `Test:` footer or `CoderReport.test_footers` "
                        "(commit_message_format rejects `:all` footers): name each migrated target explicitly "
                        "as `@//<dir>:<target>` (e.g. `fx bazel build --config=fuchsia_platform "
                        "@//<dir>:<target>`), which never applies `assert_no_deps_aspect` to "
                        f"`{' '.join(missing_excls)}` at the top level."
                    ),
                })
        # Check that `fx build` footers only pass GN labels configured in the active toolchain
        # (unqualified host-only targets fail `fx gn desc` in the default Fuchsia toolchain).
        if toolchains_map is not None and re.search(r"\bfx\s+build\b", test_cmd):
            has_host_flag = bool(re.search(r"(?:^|\s)--host(?=\s|$)", test_cmd))
            bad_lbls = []
            for lbl in re.findall(r"//[^\s,;\"'`]+", m_line):
                base_lbl = lbl.split("(", 1)[0]
                known_keys = toolchains_map.get(base_lbl, [])
                is_configured = (
                    any("host" in k for k in known_keys)
                    if (has_host_flag and "(" not in lbl)
                    else (lbl in known_keys)
                )
                if not is_configured:
                    bad_lbls.append(lbl)
            if bad_lbls:
                findings.append({
                    "source": "build_verification",
                    "category": "unconfigured_test_footer_label",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` passes GN label(s) not in the "
                        "configured default-toolchain GN graph (<build dir>/ninja_outputs.json): "
                        f"{' '.join(bad_lbls)}. `fx build` and `test_verifier` (`fx gn desc`) "
                        "fail on host-only or unconfigured labels without a configured toolchain."
                    ),
                    "remediation": (
                        "Replace `fx build` in the commit `Test:` footer and "
                        "`CoderReport.test_footers` with `fx bazel build --config=host "
                        "@//<dir>:<target>` (for host-only targets such as `ffx_plugin`, "
                        "`ffx_tool`, `*_host_tool`, or `HOST_OS_CONSTRAINTS`) or "
                        "`fx bazel build --config=fuchsia_platform @//<dir>:<target>` (for "
                        "Fuchsia targets). Using `fx bazel build` here supersedes generic "
                        "`fx build //<dir>:<target>` task-prompt instructions and is NOT a "
                        "`machinery_conflict` (never report it in `disputed_findings`)."
                    ),
                })
        # Check that fx test footers reference leaf test targets or test packages
        # rather than GN group("tests") (:tests), bazel_test_suite wrappers, or
        # non-test library targets, and verify that when <build dir>/tests.json is
        # non-empty the target is actually present in <build dir>/tests.json so
        # Planter's test_verifier (`fx test --dry`) does not fail.
        def is_structural_fx_test_label(lbl):
            pkg_d, sep, t_name = lbl[2:].partition(":")
            pkg_d = pkg_d.strip("/")
            gn_txt = gn_code(read(f"{pkg_d}/BUILD.gn") or "")
            bz_txt = gn_code(read(f"{pkg_d}/BUILD.bazel") or "")
            if sep:
                if t_name == "tests" and re.search(r'\bgroup\(\s*"tests"\s*\)', gn_txt):
                    return False
                if re.search(r'\bbazel_test_suite\(\s*"' + re.escape(t_name) + r'"\s*\)', gn_txt):
                    return False
            bz_test_re = (
                r"\b(?:fx_unittest_package|fx_unittest_component|fx_test|fx_test_package|"
                r"host_rustc_test|wrap_host_rust_test|host_go_test|host_py_test|host_test|"
                r"host_cc_test|host_shac_test|rustc_test|go_test|cc_test|test_suite)\s*\("
            )
            gn_test_re = (
                r"\b(?:fuchsia_unittest_package|fuchsia_test_package|fuchsia_test|"
                r"fuchsia_unittest_component|fuchsia_test_component|rustc_test|go_test|"
                r"test|python_host_test|host_test)\s*\("
            )
            if not sep:
                return bool(
                    re.search(bz_test_re, bz_txt)
                    or re.search(gn_test_re, gn_txt)
                    or re.search(r"\bbazel_test_suite\s*\(", gn_txt)
                    or re.search(r"\bwith_(?:host_)?unit_tests\b", bz_txt)
                    or UNIT_TESTS.search(gn_txt)
                )
            full_lbl = f"//{pkg_d}:{t_name}"
            if (
                full_lbl in exported_tests["host_tests"]
                or full_lbl in exported_tests["target_tests"]
                or re.search(r'"' + re.escape(full_lbl) + r'"', gn_txt)
            ):
                return True
            # Parse top-level BUILD.bazel rule calls via AST so single-line calls
            # (such as package(...) or exports_files(...)) do not swallow the next
            # multi-line test target block under regex scanning.
            try:
                bz_tree = ast.parse(bz_txt)
            except SyntaxError:
                bz_tree = None
            if bz_tree is not None:
                for stmt in bz_tree.body:
                    if not isinstance(stmt, ast.Expr) or not isinstance(stmt.value, ast.Call):
                        continue
                    call = stmt.value
                    rule_fn = (
                        call.func.id
                        if isinstance(call.func, ast.Name)
                        else (call.func.attr if isinstance(call.func, ast.Attribute) else "")
                    )
                    if not rule_fn:
                        continue
                    c_name = None
                    has_unit_tests = False
                    for kw in call.keywords:
                        if (
                            kw.arg == "name"
                            and isinstance(kw.value, ast.Constant)
                            and isinstance(kw.value.value, str)
                        ):
                            c_name = kw.value.value
                        elif kw.arg in ("with_unit_tests", "with_host_unit_tests"):
                            if not (
                                isinstance(kw.value, ast.Constant) and kw.value.value is False
                            ):
                                has_unit_tests = True
                    if not c_name:
                        continue
                    if c_name == t_name and re.match(bz_test_re, rule_fn + "("):
                        return True
                    if t_name == f"{c_name}_test" and has_unit_tests:
                        return True
            for m in re.finditer(r"\b([A-Za-z_]\w*)\s*\((.*?)\n\)", bz_txt, re.S):
                rule_fn, blk = m.group(1), m.group(2)
                nm = re.search(r'\bname\s*=\s*"([^"]+)"', blk)
                if not nm:
                    continue
                if nm.group(1) == t_name and re.match(bz_test_re, rule_fn + "("):
                    return True
                if (
                    t_name == f"{nm.group(1)}_test"
                    and re.search(r"\bwith_(?:host_)?unit_tests\b", blk)
                ):
                    return True
            for b_name, (tmpl, body) in gn_blocks(gn_txt).items():
                if b_name == t_name and re.match(gn_test_re, tmpl + "("):
                    return True
                if t_name == f"{b_name}_test" and tmpl.startswith("rustc_") and UNIT_TESTS.search(body):
                    return True
            return False

        def is_in_tests_json(lbl, tj_targets):
            if not tj_targets:
                return True
            _, sep, _ = lbl[2:].partition(":")
            return bool(
                lbl in tj_targets
                or any(t == lbl or (not sep and t.startswith(lbl + ":")) for t in tj_targets)
            )

        if re.search(r"\bfx\s+(?:-t\s+\S+\s+)?test\b", test_cmd):
            missing_tool = missing_fx_test_tool()
            if missing_tool is not None:
                tool_rel, tool_lbl = missing_tool
                findings.append({
                    "source": "build_verification",
                    "category": "unbuilt_fx_test_tool",
                    "severity": "ERROR",
                    "file": "COMMIT_MSG",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` runs `fx test`, "
                        f"but the `test` host tool (`{tool_rel}`) has not been built, "
                        "so Planter's `test_verifier` (`fx test --dry`) fails with "
                        "`missing_fx_test_target` (`Tool \"test\" is required by this "
                        "command and has not been built`)."
                    ),
                    "remediation": (
                        "Replace `Test: fx test ...` in the commit message and "
                        "`CoderReport.test_footers` with "
                        "`Test: fx bazel build --config=fuchsia_platform @//<dir>:<test>` "
                        "(for device tests) or `Test: fx bazel test --config=host "
                        "@//<dir>:<test>` (for host tests), or build the `fx test` host "
                        f"tool first with `fx build '{tool_lbl}'`."
                    ),
                })
            fx_test_lbls = [
                re.sub(r"^@@?//", "//", lbl).split("(", 1)[0]
                for lbl in re.findall(r"(?<![:\w])@?//[^\s,;\"'`]+", test_cmd)
            ]
            non_test_lbls = [
                lbl for lbl in fx_test_lbls if not is_structural_fx_test_label(lbl)
            ]
            unconfigured_lbls = [
                lbl for lbl in fx_test_lbls
                if lbl not in non_test_lbls and not is_in_tests_json(lbl, tests_json_targets)
            ]
            if non_test_lbls:
                findings.append({
                    "source": "build_verification",
                    "category": "unconfigured_fx_test_footer_label",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` passes non-test or "
                        f"aggregator target(s) to `fx test`: {' '.join(non_test_lbls)}. "
                        "`fx test` requires a leaf test target (exported by "
                        "`bazel_test_suite` or defined as a test rule in "
                        "`BUILD.bazel`/`BUILD.gn`) or a package directory with tests, "
                        "never a GN `group(\"tests\")` (`:tests`), `bazel_test_suite` "
                        "wrapper, or library target."
                    ),
                    "remediation": (
                        "Replace the `Test: fx test ...` footer with "
                        "`Test: fx bazel test --config=host @//<dir>:<test>` (for host tests) or "
                        "`Test: fx bazel build --config=fuchsia_platform @//<dir>:<test>` (for device tests), "
                        "or pass a leaf test target present in `<build dir>/tests.json`."
                    ),
                })
            if unconfigured_lbls:
                findings.append({
                    "source": "build_verification",
                    "category": "unconfigured_fx_test_footer_label",
                    "severity": "ERROR",
                    "file": "",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line.strip()}` passes target(s) to `fx test` "
                        f"that are not in the configured `<build dir>/tests.json`: {' '.join(unconfigured_lbls)}. "
                        "When `<build dir>/tests.json` is non-empty (configured for a different area's tests in `args.gn`), "
                        "Planter's `test_verifier` runs `fx test --dry` and fails with `missing_fx_test_target` on any "
                        "`fx test` label or package path outside `<build dir>/tests.json`."
                    ),
                    "remediation": (
                        "Do NOT use `Test: fx test ...` when the target is not in `<build dir>/tests.json`. "
                        "Amend the commit `Test:` footer and `CoderReport.test_footers` to use "
                        "`Test: fx bazel test --config=host @//<dir>:<test>` (for host tests) or "
                        "`Test: fx bazel build --config=fuchsia_platform @//<dir>:<test>` (for device tests like `fx_test` / "
                        "`fx_unittest_package`). Using `fx bazel test|build` when `<build dir>/tests.json` omits the target "
                        "is the required convention, not a `machinery_conflict` (do not report it in `disputed_findings`)."
                    ),
                })
    if len(test_footer_lines) > 3:
        findings.append({
            "source": "build_verification",
            "category": "too_many_test_footer_lines",
            "severity": "ERROR",
            "file": "COMMIT_MSG",
            "line": 0,
            "message": (
                f"Commit message has {len(test_footer_lines)} `Test:` footer lines (at most 3 allowed). "
                "Per-package incremental build or query lines clutter the commit message when more than 3 `Test:` lines are present."
            ),
            "remediation": (
                "Consolidate the commit message `Test:` footers (and `CoderReport.test_footers`) to at most 3 lines: "
                "one `fx bazel test|build --config=fuchsia_platform @//<dir>:<test>` line for the migrated test targets, "
                "one `fx bazel test --config=host @//<dir>:<host test>` line when host tests exist, and "
                "`fx lint` last; "
                "remove redundant per-package `fx build` / `fx bazel build` / `fx bazel query` lines."
            ),
        })

STEP_BLOCKING = ("ERROR", "WARNING")
BUILD_FILE_NAMES = {"BUILD.bazel", "BUILD", "BUILD.gn", "MODULE.bazel"}
BUILD_FILE_EXTS = (".bazel", ".bzl", ".gn", ".gni")


def _subrepo_dirs():
    subrepos = []
    vg = os.path.join(workdir, "vendor", "google")
    if os.path.exists(os.path.join(vg, ".git")):
        subrepos.append(vg)
    for d in dirs:
        abs_d = os.path.join(workdir, d)
        if not os.path.isdir(abs_d):
            continue
        try:
            top = subprocess.check_output(
                ["git", "-C", abs_d, "rev-parse", "--show-toplevel"],
                stderr=subprocess.DEVNULL, text=True,
            ).strip()
            if top and os.path.realpath(top) != os.path.realpath(workdir) and top not in subrepos:
                subrepos.append(top)
        except Exception:
            pass
    return subrepos


def _subrepos_clean_since_upload():
    for sr in _subrepo_dirs():
        try:
            diff_sr = subprocess.check_output(
                ["git", "-C", sr, "diff", "--name-only", "HEAD", "--"],
                stderr=subprocess.DEVNULL, text=True,
            ).strip()
            if diff_sr:
                return False
            untracked_sr = [
                u.strip() for u in subprocess.check_output(
                    ["git", "-C", sr, "ls-files", "--others", "--exclude-standard"],
                    stderr=subprocess.DEVNULL, text=True,
                ).splitlines() if u.strip()
            ]
            if any(
                os.path.basename(u) in BUILD_FILE_NAMES or u.endswith(BUILD_FILE_EXTS)
                for u in untracked_sr
            ):
                return False
        except Exception:
            return False
    return True


# Check whether the host `bazel2gn` binary executed by `shac check` (`<build dir>/host_*/bazel2gn`)
# is missing or older than `//build/tools/bazel2gn` sources or `bazel-bin/.../bazel2gn`.
def _bazel2gn_host_tool_stale():
    build_dir = (read(".fx-build-dir") or "").strip()
    if not build_dir:
        return False
    host_bins = [
        os.path.join(workdir, build_dir, "host_x64", "bazel2gn"),
        os.path.join(workdir, build_dir, "host_arm64", "bazel2gn"),
    ]
    existing_hosts = [p for p in host_bins if os.path.isfile(p)]
    if not existing_hosts:
        return True
    host_mtime = max(os.path.getmtime(p) for p in existing_hosts)
    b2gn_src_dir = os.path.join(workdir, "build", "tools", "bazel2gn")
    if os.path.isdir(b2gn_src_dir):
        for root, _, files in os.walk(b2gn_src_dir):
            for fn in files:
                if fn.endswith((".go", ".json", ".gni", ".bzl", ".bazel")):
                    try:
                        if os.path.getmtime(os.path.join(root, fn)) > host_mtime:
                            return True
                    except OSError:
                        pass
    bazel_bin = os.path.join(
        workdir,
        build_dir,
        "gen",
        "build",
        "bazel",
        "workspace",
        "bazel-bin",
        "build",
        "tools",
        "bazel2gn",
        "bazel2gn_",
        "bazel2gn",
    )
    try:
        if os.path.isfile(bazel_bin) and os.path.getmtime(bazel_bin) > host_mtime:
            return True
    except OSError:
        pass
    return False


uploaded_commit = os.environ.get("PLANTER_UPLOADED_COMMIT", "").strip()
if uploaded_commit and not dry_run and os.environ.get("PLANTER_BUILD_NO_REUSE", "").strip() in ("", "0"):
    try:
        up_tree = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", f"{uploaded_commit}^{{tree}}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
        head_tree = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "HEAD^{tree}"],
            stderr=subprocess.DEVNULL, text=True,
        ).strip()
    except Exception:
        up_tree, head_tree = "", ""
    diff_worktree = git_lines(["diff", "--name-only", "HEAD", "--"])
    untracked_in_dirs = [
        u for u in git_lines(["ls-files", "--others", "--exclude-standard"])
        if any(u == d or u.startswith(d + "/") for d in dirs)
        or os.path.basename(u) in BUILD_FILE_NAMES
        or u.endswith(BUILD_FILE_EXTS)
    ]
    if (
        up_tree
        and up_tree == head_tree
        and not diff_worktree
        and not untracked_in_dirs
        and _subrepos_clean_since_upload()
        and not _bazel2gn_host_tool_stale()
    ):
        findings.append({
            "source": "build_verification",
            "category": "build_reused",
            "severity": "INFO",
            "message": f"No files changed since uploaded patchset ({uploaded_commit[:12]}); only commit message Test: footers were verified.",
        })
        print(json.dumps(findings, indent=2))
        sys.exit(0)

reuse_path = None
reuse_key = None
reused = None
reused_time = 0
if (
    not dry_run
    and not skip_build
    and os.environ.get("PLANTER_BUILD_NO_REUSE", "").strip() in ("", "0")
):
    def _git_bytes(args, repo_dir=workdir):
        return subprocess.run(["git", "-C", repo_dir] + args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=True).stdout

    def _file_digest(path):
        h = hashlib.sha256()
        try:
            if os.path.islink(path):
                h.update(b"link:" + os.readlink(path).encode())
            else:
                with open(path, "rb") as f:
                    for chunk in iter(lambda: f.read(1 << 20), b""):
                        h.update(chunk)
        except OSError:
            h.update(b"missing")
        return h.hexdigest()

    def _worktree_tree(repo_dir=workdir):
        # The git tree of the whole working tree (tracked edits and untracked, non-ignored files),
        # so committing the change after a run does not change the fingerprint. Uses a copy of
        # the index so the real one is untouched and its stat cache keeps this fast.
        subprocess.run(["git", "-C", repo_dir, "update-index", "-q", "--refresh"],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        dirty_out = subprocess.run(
            ["git", "-C", repo_dir, "diff", "--name-only", "--ignore-submodules=all", "HEAD"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
        ).stdout
        untracked_out = subprocess.run(
            ["git", "-C", repo_dir, "ls-files", "--others", "--exclude-standard"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True,
        ).stdout
        dirty = sorted({
            l.strip() for l in (dirty_out.splitlines() + untracked_out.splitlines()) if l.strip()
        })
        if not dirty:
            return _git_bytes(["rev-parse", "HEAD^{tree}"], repo_dir=repo_dir)
        git_dir = _git_bytes(["rev-parse", "--absolute-git-dir"], repo_dir=repo_dir).decode().strip()
        fd, index = tempfile.mkstemp(dir=git_dir, prefix=".planter-build-verification-index.")
        os.close(fd)
        try:
            try:
                shutil.copyfile(os.path.join(git_dir, "index"), index)
            except OSError:
                os.unlink(index)
            env = dict(os.environ, GIT_INDEX_FILE=index)
            subprocess.run(["git", "-C", repo_dir, "add", "-A", "--"] + dirty, env=env, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, check=True)
            return subprocess.run(["git", "-C", repo_dir, "write-tree"], env=env, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, check=True).stdout
        finally:
            try:
                os.unlink(index)
            except OSError:
                pass

    def _fingerprint():
        h = hashlib.sha256()

        def _add(label, data):
            if isinstance(data, str):
                data = data.encode()
            h.update(label.encode() + b"\0" + str(len(data)).encode() + b"\0" + data)

        _add("tree", _worktree_tree(workdir))
        head_msg = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
        _add(
            "head_submitted",
            "1" if ("Reviewed-on:" in head_msg or "Roller-URL:" in head_msg) else "0",
        )
        for sr in _subrepo_dirs():
            try:
                rel_sr = os.path.relpath(sr, workdir)
                _add(f"subrepo:{rel_sr}", _worktree_tree(sr))
            except Exception:
                pass
        _add("dirs", json.dumps([sorted(dirs), sorted(bazel_dirs), sorted(host_dirs)]))
        _add("steps", json.dumps([[n, c] for n, c in steps]))
        _add("env", json.dumps([timeout, dependent_limit]))
        build_dir = (read(".fx-build-dir") or "").strip()
        _add("build_dir", build_dir)
        if build_dir:
            _add("args.gn", _file_digest(os.path.join(workdir, build_dir, "args.gn")))
            for hb in ("host_x64/bazel2gn", "host_arm64/bazel2gn"):
                hb_path = os.path.join(workdir, build_dir, hb)
                if os.path.isfile(hb_path):
                    _add(f"b2gn:{hb}", _file_digest(hb_path))
        fx_bin = next(
            (
                os.path.join(workdir, rel)
                for rel in (".jiri_root/bin/fx", "scripts/fx")
                if os.path.isfile(os.path.join(workdir, rel))
            ),
            None,
        )
        if fx_bin:
            try:
                gn_ver = subprocess.check_output(
                    [fx_bin, "gn", "--version"],
                    cwd=workdir,
                    stderr=subprocess.DEVNULL,
                    text=True,
                    timeout=10,
                ).strip()
                _add("gn_version", gn_ver)
            except Exception:
                pass
        _add("jiri", _file_digest(os.path.join(workdir, ".jiri_root", "update_history", "latest")))
        return h.hexdigest()

    try:
        git_dir = _git_bytes(["rev-parse", "--absolute-git-dir"]).decode().strip()
        reuse_path = os.path.join(git_dir, "planter-build-verification.json")
        reuse_key = _fingerprint()
        ttl = int(os.environ.get("PLANTER_BUILD_REUSE_TTL", "21600"))
        with open(reuse_path) as f:
            rec = json.load(f)
        entry = None
        if isinstance(rec.get("entries"), dict):
            entry = rec["entries"].get(reuse_key)
        if entry is None and rec.get("key") == reuse_key:
            entry = rec
        if isinstance(entry, dict) and 0 <= time.time() - entry.get("time", 0) <= ttl:
            cached = entry.get("findings")
            if isinstance(cached, list) and not any(c.get("severity") in STEP_BLOCKING for c in cached):
                reused = cached
                reused_time = entry.get("time", 0)
    except (OSError, ValueError, subprocess.CalledProcessError):
        pass

if reused is not None or (skip_build and not dry_run):
    unconf_note = unconfigured_own_gn_note(unconfigured_own_gn_labels(load_toolchains()[0]))
    if unconf_note and not any(
        f.get("category") == "dependents_note" and f.get("message") == unconf_note
        for f in (reused or [])
    ):
        findings.append({
            "source": "build_verification",
            "category": "dependents_note",
            "severity": "INFO",
            "message": unconf_note,
        })

# Check that commit Test: footers do not run a Bazel config blocked by an external repo failure.
def check_ext_repo_failed_test_footers(step_items, footer_lines, target_pkg_dirs):
    ext_cfgs = set()
    for item in step_items or []:
        if not isinstance(item, dict):
            continue
        cat = item.get("category", "")
        if cat == "preexisting_failure_bazel_build_fuchsia":
            ext_cfgs.add("fuchsia_platform")
        elif cat == "preexisting_failure_bazel_build_host":
            ext_cfgs.add("host")
    if not ext_cfgs or not footer_lines:
        return
    for m_line in footer_lines:
        for cfg in sorted(ext_cfgs):
            if f"--config={cfg}" not in m_line or not re.search(
                r"\bfx\s+bazel\s+(?:build|test)\b", m_line
            ):
                continue
            if any(f"//{d}" in m_line for d in target_pkg_dirs):
                findings.append({
                    "source": "build_verification",
                    "category": "test_footer_external_repo_failure",
                    "severity": "ERROR",
                    "file": "COMMIT_MSG",
                    "line": 0,
                    "message": (
                        f"Commit `Test:` footer `{m_line}` runs `fx bazel` with "
                        f"`--config={cfg}`, which exits non-zero due to a pre-existing "
                        "upstream or stacked parent build failure and causes "
                        "`test_verifier` to fail with `test_footer_command_failed`."
                    ),
                    "remediation": (
                        f"Amend the commit message and `CoderReport.test_footers` to "
                        f"replace `{m_line}` with runnable verification command(s) that "
                        "exit 0 (such as `Test: fx build //<dir>:tests`, "
                        "`Test: fx lint`, or "
                        "`Test: fx bazel build --config=host` for host-compatible "
                        "targets), note the pre-existing upstream/prerequisite build "
                        "limitation in the commit body, and report with "
                        "`disputed_findings: []`."
                    ),
                })


if reused is not None:
    # Refresh cached bazel_tests_note / dependents_note wording when reusing a build result.
    for rf in reused:
        if isinstance(rf, dict) and rf.get("category") in ("bazel_tests_note", "bazel_tests_exported"):
            continue
        findings.append(rf)
    if exported_tests["host_tests"] or exported_tests["target_tests"]:
        for n in bazel_tests_export_notes():
            findings.append({
                "source": "build_verification",
                "category": "bazel_tests_exported",
                "severity": "INFO",
                "message": n,
            })
    findings.append({
        "source": "build_verification",
        "category": "build_reused",
        "severity": "INFO",
        "message": "Reused the passing build result recorded for this exact change "
                   f"({time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(reused_time))}); "
                   "set PLANTER_BUILD_NO_REUSE=1 to rebuild.",
    })
    check_ext_repo_failed_test_footers(reused, locals().get("test_footer_lines", []), dirs)
    print(json.dumps(findings, indent=2))
    sys.exit(0)

if skip_build and not dry_run:
    if exported_tests["host_tests"] or exported_tests["target_tests"]:
        for n in bazel_tests_export_notes():
            findings.append({
                "source": "build_verification",
                "category": "bazel_tests_exported",
                "severity": "INFO",
                "message": n,
            })
    findings.append({
        "source": "build_verification",
        "category": "build_skipped",
        "severity": "INFO",
        "message": "PLANTER_SKIP_BUILD is set; the migration was NOT built.",
    })
    print(json.dumps(findings, indent=2))
    sys.exit(0)

FROM_TARGET = re.compile(r"\(from target (?:@@?[\w.~+-]*)?(//[^\s)]+)\)")
NINJA_STEP_TARGET = re.compile(
    r"^\[[0-9]+/[0-9]+\](?:\([0-9]+\))?\s+\S+\s+(//([A-Za-z0-9_./+-]+):([^\s(]+))"
)
# Extract the transitive dependency manifest path from clippy or rustc Ninja command lines.
TRANSDEPS_ARG = re.compile(r"(?:--transdeps\s+|--rust-compilation-deps-path=|@shell:)(\S+)")
# Extract the library_infos manifest path from Python mypy or build-time-test Ninja command lines.
PYTHON_LIB_INFOS_ARG = re.compile(r"--library[_-]infos\s+(\S+)")
# Extract the GN package and target name from a failed Ninja `.mypy_checked` or `.check` output.
NINJA_FAILED_PYTHON_STAMP = re.compile(
    r"^FAILED:\s*(?:\[code=\d+\]\s+)?(?:host_[A-Za-z0-9_]+/)?obj/"
    r"([A-Za-z0-9_./+-]+)/([^/\s]+)\.(?:mypy_checked|check)\b"
)
NINJA_MISSING_RULE = re.compile(
    r"^ninja: error: '([^']+)', needed by '([^']+)', missing and no known rule to make it$"
)
NINJA_MISSING_PREBUILT = re.compile(
    r"(?:\.\./)+(prebuilt/[A-Za-z0-9_./+-]+):\s*No such file or directory"
)


# Check whether Ninja failed because a CIPD prebuilt tool under `prebuilt/` is missing on disk
# (for example after rebasing `fuchsia.git` onto a newer `origin/main` without `jiri update`).
def unrelated_ninja_missing_prebuilt(lines):
    failed_indices = [i for i, l in enumerate(lines) if l.startswith("FAILED:")]
    if not failed_indices:
        return None
    missing_prebuilts = []
    for idx in failed_indices:
        window = "\n".join(lines[idx : min(len(lines), idx + 8)])
        matches = NINJA_MISSING_PREBUILT.findall(window)
        if not matches:
            return None
        for rel_prebuilt in matches:
            rel_prebuilt = rel_prebuilt.strip("/")
            if os.path.exists(os.path.join(workdir, rel_prebuilt)):
                return None
            if rel_prebuilt not in missing_prebuilts:
                missing_prebuilts.append(rel_prebuilt)
    if not missing_prebuilts:
        return None
    return (
        f"Ninja failed on missing prebuilt tool(s) ({' '.join(missing_prebuilts)}) in the "
        "checkout (e.g. after rebasing onto a newer `origin/main` revision whose CIPD "
        "packages have not been fetched). Keep the commit based on the checkout's "
        "jiri-synced revision unless a rebase is required; the changed packages are still "
        "verified by the remaining steps."
    )


# Check whether a single failed Ninja step is in an unrelated GN target with no dependency path
# to any changed package (via Rust transdeps or Python mypy/build-time-test library_infos).
def unrelated_ninja_step_target(lines, idx, build_dir, rdep_pkgs):
    pm = NINJA_FAILED_PYTHON_STAMP.match(lines[idx].strip())
    sm = NINJA_STEP_TARGET.match(lines[idx - 1].strip()) if idx > 0 else None
    if pm:
        pkg = pm.group(1).strip("/")
        full_lbl = f"//{pkg}:{pm.group(2)}"
    elif sm:
        full_lbl, pkg = sm.group(1), sm.group(2).strip("/")
    else:
        return None
    if any(pkg == d or pkg.startswith(d + "/") or d.startswith(pkg + "/") for d in dirs):
        return None
    if pkg in rdep_pkgs or git_lines(["diff", "--name-only", change_base, "--", pkg]):
        return None
    cmd_line = lines[idx + 1] if idx + 1 < len(lines) else ""
    tm = TRANSDEPS_ARG.search(cmd_line)
    if tm and build_dir:
        t_content = read(os.path.join(build_dir, tm.group(1)))
        if t_content is None or any(f"/{d}/" in t_content for d in dirs):
            return None
        return full_lbl
    lm = PYTHON_LIB_INFOS_ARG.search(cmd_line)
    if lm or pm:
        if any(f"/{d}/" in cmd_line for d in dirs):
            return None
        if lm and build_dir:
            l_content = read(os.path.join(build_dir, lm.group(1)))
            if l_content is not None and any(f"/{d}/" in l_content for d in dirs):
                return None
        return full_lbl
    return None


# Check whether Ninja aborted on missing source files in an out-of-sync submodule or
# subrepository outside the changed packages and their reverse dependencies.
def unrelated_ninja_missing_source(lines, rdep_pkgs):
    ninja_errors = [l.strip() for l in lines if "ninja: error:" in l]
    if not ninja_errors or any(l.startswith("FAILED:") for l in lines):
        return None
    missing_srcs = []
    for err_line in ninja_errors:
        mm = NINJA_MISSING_RULE.match(err_line)
        if not mm or not mm.group(1).startswith("../"):
            return None
        rel_src = re.sub(r"^(?:\.\./)+", "", mm.group(1)).strip("/")
        needed_by = mm.group(2).strip("/")
        if not rel_src or os.path.exists(os.path.join(workdir, rel_src)):
            return None
        if any(
            rel_src == d
            or rel_src.startswith(d + "/")
            or f"/{d}/" in f"/{needed_by}/"
            for d in dirs
        ):
            return None
        if any(rel_src == p or rel_src.startswith(p + "/") for p in rdep_pkgs):
            return None
        if git_lines(["diff", "--name-only", change_base, "--", rel_src]):
            return None
        if rel_src not in missing_srcs:
            missing_srcs.append(rel_src)
    if not missing_srcs:
        return None
    return (
        f"Ninja failed on missing source file(s) ({' '.join(missing_srcs)}) outside the "
        "changed packages and their GN reverse dependencies (out-of-sync submodule or "
        "subrepository in the checkout). Do not edit them; the changed packages and their "
        "GN dependents are still verified by the remaining steps."
    )


# Normalize a file path from `regenerator.py` / Bazel workspace output to a checkout-relative path.
def _norm_regenerator_workspace_path(raw_p):
    p = (raw_p or "").strip().replace("../../", "", 1)
    if p.startswith(workdir + "/"):
        p = p[len(workdir) + 1 :]
    ws_marker = "/gen/build/bazel/workspace/"
    if ws_marker in p:
        p = p.split(ws_marker, 1)[1]
    return os.path.normpath(p).strip("/")


# Check whether `fx build` failed in `regenerator.py` (`build.ninja.stamp`) solely because
# the `tests.json` Bazel query encountered errors in packages outside the current change
# (such as a stacked parent commit or pre-existing upstream breakage), after `gn gen` succeeded.
def unrelated_regenerator_bazel_query_failure(output):
    if not bazel_dirs or any(p.endswith((".gni", ".bzl", "MODULE.bazel")) for p in changed):
        return None
    if re.search(r"ERROR at //", output) or not re.search(
        r"Done\.\s+Made\s+\d+\s+targets\b", output
    ):
        return None
    if "failed to generate tests.json: Failed to run bazel query:" not in output:
        return None
    lines = output.splitlines()
    failed_lines = [l.strip() for l in lines if l.startswith("FAILED:")]
    if not failed_lines or any(
        not re.match(r"^FAILED:\s*(?:\[code=\d+\]\s+)?build\.ninja\.stamp\b", l)
        for l in failed_lines
    ):
        return None
    raw_paths = []
    for m in re.finditer(r'File\s+"([^"]+)"', output):
        raw_paths.append(m.group(1))
    for m in re.finditer(
        r"(?:^|[\s'\"(])((?:/[^\s:'\"]+|(?:\.\./\.\./)?[\w./+-]+)\.(?:bazel|bzl|gn|gni))(?::|\(|,|\s)",
        output,
        re.M,
    ):
        raw_paths.append(m.group(1))
    for m in LOCATION.finditer(output):
        raw_paths.append(m.group(1))
    unrelated_files = []
    for rp in raw_paths:
        np = _norm_regenerator_workspace_path(rp)
        if not np or np.startswith(("out/", "bazel-out/", "external/")):
            continue
        if any(np == d or np.startswith(d + "/") for d in dirs):
            return None
        if (
            np in changed_set
            and not _owned_by_stacked_parent_commit(np)
            and not _is_ancestor_test_wiring_only(os.path.dirname(np).strip("/"))
        ):
            return None
        if os.path.basename(np) in ("BUILD.bazel", "BUILD.gn") and np not in unrelated_files:
            unrelated_files.append(np)
    for m in re.finditer(r"//([A-Za-z0-9_./+-]+)(?::([A-Za-z0-9_./+-]+))?", output):
        pkg = m.group(1).strip("/")
        if any(pkg == d or pkg.startswith(d + "/") for d in dirs):
            return None
    if not unrelated_files:
        return None
    shown = " ".join(unrelated_files)
    return (
        f"GN generation succeeded, and the `tests.json` Bazel query in `regenerator.py` "
        f"failed only in unrelated package(s) ({shown}) outside the changed packages "
        "(e.g. a stacked parent commit or pre-existing upstream breakage). Do not edit "
        "unrelated packages or report this pre-existing failure in `disputed_findings` "
        "(set `disputed_findings` to `[]` in `CoderReport` and list only exit-0 commands "
        "such as `fx bazel build` in `Test:` footers); the changed packages are verified "
        "by the remaining steps."
    )


# Check whether a native Ninja failure in `fx build` is in an unrelated GN target
# outside the changed packages and their reverse dependencies.
def unrelated_gn_ninja_failure(output):
    if not bazel_dirs or any(p.endswith((".gni", ".bzl", "MODULE.bazel")) for p in changed):
        return None
    if re.search(r"ERROR at //", output):
        return None
    for m in LOCATION.finditer(output):
        loc_path = m.group(1).replace("../../", "", 1)
        if any(loc_path == d or loc_path.startswith(d + "/") for d in dirs):
            return None
    regen_note = unrelated_regenerator_bazel_query_failure(output)
    if regen_note:
        return regen_note
    rdep_pkgs = {os.path.dirname(f) for f in rdep_build_files()}
    lines = output.splitlines()
    if any("ninja: error:" in l for l in lines):
        return unrelated_ninja_missing_source(lines, rdep_pkgs)
    missing_prebuilt_note = unrelated_ninja_missing_prebuilt(lines)
    if missing_prebuilt_note:
        return missing_prebuilt_note
    build_dir = (read(".fx-build-dir") or "").strip()
    failed_targets = []
    for i, l in enumerate(lines):
        if not l.startswith("FAILED:"):
            continue
        full_lbl = unrelated_ninja_step_target(lines, i, build_dir, rdep_pkgs)
        if not full_lbl:
            return None
        if full_lbl not in failed_targets:
            failed_targets.append(full_lbl)
    if not failed_targets:
        return None
    return (
        f"the failing GN targets ({' '.join(failed_targets)}) are outside the changed packages "
        "and have no dependency path to any changed package, so the failure predates the change "
        "(pre-existing upstream build failure). Do not edit them; the changed packages and their "
        "GN dependents are still verified by the remaining steps."
    )


# Detect Bazel failures confined to external repository targets outside the main repo.
FROM_FULL_TARGET = re.compile(r"\(from target (@@?[\w.~+-]*)?(//[^\s)]+)\)")


# Collect header directory prefixes from third_party_target_map.json external repos.
def _external_repo_header_prefixes():
    prefixes = set()
    map_path = os.path.join(
        workdir, "build", "tools", "bazel2gn", "third_party_target_map.json"
    )
    try:
        with open(map_path, encoding="utf-8") as mf:
            mapping = json.load(mf)
        for gn_lbl in mapping.values():
            if isinstance(gn_lbl, str) and gn_lbl.startswith("//third_party/"):
                sub = gn_lbl[len("//third_party/") :].split(":", 1)[0].strip("/")
                if sub:
                    prefixes.add(sub.split("/", 1)[0])
    except (OSError, ValueError):
        pass
    return prefixes


# Collect compile error/fatal-error diagnostic lines in Bazel output that are not inside
# an external repository (`external/...`).
def _main_repo_compile_diag_lines(output):
    return [
        ln
        for ln in output.splitlines()
        if (": error:" in ln or ": fatal error:" in ln) and "external/" not in ln
    ]


# Match a `<subdir>/<header>` file-not-found diagnostic, capturing the top-level subdir.
_SUBDIR_HEADER_NOT_FOUND = re.compile(
    r"['\"<]([A-Za-z0-9_+-]+)/[^'\">]+['\">] file not found"
)


# Detect whether every non-external compile diagnostic in Bazel output is an
# angled or subdirectory header-not-found error from an external repository
# lacking `includes` in its upstream BUILD.bazel.
def _only_ext_angled_include_main_diags(output):
    non_ext = _main_repo_compile_diag_lines(output)
    if not non_ext:
        return True
    ext_prefixes = _external_repo_header_prefixes()
    for ln in non_ext:
        if "file not found with <angled> include" in ln:
            continue
        m_hdr = _SUBDIR_HEADER_NOT_FOUND.search(ln)
        if m_hdr and m_hdr.group(1) in ext_prefixes:
            continue
        return False
    return True


# Check whether changed files wire a migrated package into a Bazel test_suite
# or GN bazel_test_suite that executes inside `fx build` on CQ builders.
def _changed_wires_bazel_test_suite():
    for rel_f in changed:
        base_f = os.path.basename(rel_f)
        if base_f not in ("BUILD.bazel", "BUILD.gn"):
            continue
        diff_txt = "\n".join(git_lines(["diff", "-U0", change_base, "--", rel_f]))
        for raw_ln in diff_txt.splitlines():
            if not raw_ln.startswith("+") or raw_ln.startswith("+++"):
                continue
            added = raw_ln[1:].strip()
            if any(f"//{d}:" in added for d in dirs):
                return True
    return False


# Collect third-party external repository targets inferred when main-repo C/C++ compile
# actions fail exclusively on angled/subdir includes from an external repo lacking `includes`.
def _matched_ext_angled_include_prefixes(output):
    non_ext = _main_repo_compile_diag_lines(output)
    if not non_ext or not _only_ext_angled_include_main_diags(output):
        return []
    ext_prefixes = _external_repo_header_prefixes()
    matched = []
    for ln in non_ext:
        m_hdr = _SUBDIR_HEADER_NOT_FOUND.search(ln)
        if m_hdr and m_hdr.group(1) in ext_prefixes:
            pref = f"//third_party/{m_hdr.group(1)}"
            if pref not in matched:
                matched.append(pref)
    return matched


def external_repo_bazel_failure(output):
    full_matches = FROM_FULL_TARGET.findall(output)
    if not full_matches:
        return None
    ext_targets = []
    main_targets = []
    for repo_prefix, pkg_label in full_matches:
        repo_name = (repo_prefix or "").lstrip("@")
        if repo_name:
            full_lbl = f"{repo_prefix}{pkg_label}"
            if full_lbl not in ext_targets:
                ext_targets.append(full_lbl)
        else:
            if pkg_label not in main_targets:
                main_targets.append(pkg_label)
    inferred_ext = _matched_ext_angled_include_prefixes(output) if not ext_targets else []
    effective_ext = ext_targets or inferred_ext
    if not effective_ext:
        return None
    if main_targets and not _only_ext_angled_include_main_diags(output):
        return None
    for m in LOCATION.finditer(output):
        loc_path = m.group(1).replace("../../", "", 1)
        if loc_path.startswith(workdir + "/"):
            loc_path = loc_path[len(workdir) + 1 :]
        if "external/" in loc_path:
            continue
        if (
            any(loc_path == d or loc_path.startswith(d + "/") for d in dirs)
            or loc_path in changed_set
        ):
            if not _only_ext_angled_include_main_diags(output):
                return None
    shown_ext = " ".join(effective_ext)
    return (
        f"all failing Bazel compile/link actions are inside external repository target(s) "
        f"({shown_ext}) outside the main checkout, while all main-repository targets in "
        "the migrated package(s) analyzed cleanly. Upstream external `BUILD.bazel` files "
        "under `third_party/` cannot be edited in a scoped package migration CL "
        "(`scoping_changelists.md`). Keep the migrated `BUILD.bazel` targets, set "
        "`disputed_findings` to `[]`, and list only exit-0 commands in commit `Test:` "
        "footers and `CoderReport.test_footers` (such as `Test: fx lint` "
        "or `Test: fx bazel build --config=host` when host-compatible)."
    )


# Return targeted remediation when gn_build fails inside an upstream external repository
# only because the change wired a dual-built package's test into a Bazel test_suite.
def wired_suite_external_repo_remediation(step_name, output):
    if step_name not in ("gn_build", "gn_build_dependents"):
        return None
    if not _changed_wires_bazel_test_suite():
        return None
    if not external_repo_bazel_failure(output):
        return None
    has_sentinel = any(
        "## BAZEL2GN SENTINEL" in (read(f"{d}/BUILD.gn") or "") for d in dirs
    )
    if not has_sentinel:
        return None
    explanation = (
        "All failing compile/link actions are in upstream external repository targets under "
        "`third_party/` (outside the main repository), which were pulled into `fx build` only "
        "because this change wired a dual-built package's test target into an ancestor "
        "`BUILD.bazel` `test_suite` (or `BUILD.gn` `bazel_test_suite`). Because `BUILD.gn` "
        "retains `## BAZEL2GN SENTINEL`, the generated GN test package already builds and runs "
        "in `fx build` via GN. Revert the ancestor `BUILD.bazel` `test_suite` addition (while "
        "keeping `visibility` entries required by existing GN callers in parent `BUILD.gn` "
        "files). "
    )
    # Same verdict and remediation as cq_reachability / migration_sanity
    # (planter_check_lib.external_repo_blocked_test_remediation).
    if _pcl is not None and hasattr(_pcl, "external_repo_blocked_test_remediation"):
        shared = " ".join(
            _pcl.external_repo_blocked_test_remediation(d, "the Bazel test") for d in dirs
        )
    else:
        shared = (
            "Keep the migrated `BUILD.bazel` targets synced with `BUILD.gn` and list only "
            "exit-0 commands in commit `Test:` footers and `CoderReport.test_footers`."
        )
    return (
        explanation
        + shared
        + " Do not dispute this failure; if cq_reachability or migration_sanity still asks to "
        "export the Bazel test, dispute only that finding as a check contradiction (once no "
        "other blocking findings remain), citing this external-repository failure."
    )


def unrelated_bazel_failure(output):
    """Returns a note when `fx build` failed only in Bazel actions of targets that live in another git
    repository than the change and have no Bazel dependency path to any changed package (typically a
    checkout whose repositories are out of sync), in external repository targets whose upstream
    BUILD.bazel lacks Fuchsia target deps, or in native GN/Ninja actions whose transitive deps do
    not include any changed package, else None."""
    if not _changed_wires_bazel_test_suite():
        ext_note = external_repo_bazel_failure(output)
        if ext_note:
            return ext_note
    targets = sorted(set(FROM_TARGET.findall(output)))
    if not targets:
        return unrelated_gn_ninja_failure(output)
    if not bazel_dirs:
        return None
    if any(p.endswith((".bzl", "MODULE.bazel")) for p in changed):
        return None
    lines = output.splitlines()
    for i, l in enumerate(lines):
        if l.startswith("FAILED:") and not (i + 1 < len(lines) and "bazel_ninja_delayed_actions.py" in lines[i + 1]):
            return None
    main_top = "".join(git_lines(["rev-parse", "--show-toplevel"]))
    for t in targets:
        path = os.path.join(workdir, t[2:].split(":")[0])
        if not os.path.isdir(path):
            return None
        try:
            top = subprocess.check_output(["git", "-C", path, "rev-parse", "--show-toplevel"],
                                          stderr=subprocess.DEVNULL, text=True).strip()
        except Exception:
            return None
        if not top or top == main_top:
            return None
    expr = "somepath(set({}), set({}))".format(" ".join(targets), " ".join(f"//{d}:*" for d in bazel_dirs))
    try:
        proc = subprocess.run([fx, "bazel", "query", expr], cwd=workdir, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, text=True, timeout=900)
    except subprocess.TimeoutExpired:
        return None
    if proc.returncode != 0 or proc.stdout.strip():
        return None
    return (f"the failing Bazel targets ({' '.join(targets)}) are in another git repository and have no "
            "dependency path to a changed package, so the failure predates the change (out-of-sync checkout). "
            "Do not edit them; the changed packages are still verified by the remaining steps.")


# Check whether a `bazel_build_host` or `bazel_build_fuchsia` failure is solely in
# source compilation of untouched source files for a target (upstream dependency or
# bazel2gn-verified unconfigured dead-code package) that is not built for that
# platform in the configured GN graph (<build dir>/ninja_outputs.json).
def unrelated_upstream_bazel_compile_failure(step_name, output):
    targets = sorted(set(FROM_TARGET.findall(output)))
    if not targets:
        return None
    if re.search(
        r"no such (?:target|package)|is not visible|Unable to load|"
        r"cycle in dependency graph|Analysis of target .* failed|"
        r"Traceback \(most recent call last\)",
        output,
        re.I,
    ):
        return None
    err_lines = [
        l.strip()
        for l in output.splitlines()
        if l.strip().startswith("ERROR:")
        and not l.strip().startswith("ERROR: Build did NOT complete successfully")
    ]
    if not err_lines or any("(from target " not in l for l in err_lines):
        return None
    toolchains, _ = load_toolchains()
    if toolchains is None:
        return None
    changed_pkgs = {_owning_pkg_dir(f) for f in changed_set}
    t_pkgs = set()
    for t in targets:
        t_pkg, t_name = _split_bazel_label(t)
        if not t_pkg or not t_name:
            return None
        tc_keys = toolchains.get(f"//{t_pkg}:{t_name}", [])
        if step_name == "bazel_build_host":
            if any("host" in k for k in tc_keys):
                return None
        else:
            if any("host" not in k for k in tc_keys):
                return None
        if t_pkg in dirs or t_pkg in changed_pkgs:
            if "shac_check" in failed or not _existed_at_change_base(f"{t_pkg}/BUILD.gn"):
                return None
            gn_txt = read(f"{t_pkg}/BUILD.gn") or ""
            if "## BAZEL2GN SENTINEL" not in gn_txt:
                return None
            for gn_nm in gn_blocks(gn_txt):
                gn_keys = toolchains.get(f"//{t_pkg}:{gn_nm}", [])
                if step_name == "bazel_build_host" and any("host" in k for k in gn_keys):
                    return None
                if step_name != "bazel_build_host" and any("host" not in k for k in gn_keys):
                    return None
        else:
            if not _existed_at_change_base(f"{t_pkg}/BUILD.bazel"):
                return None
        t_pkgs.add(t_pkg)
    ws_marker = "/gen/build/bazel/workspace/"
    src_exts = (".rs", ".cc", ".c", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".go")
    has_upstream_src_loc = False
    for m in LOCATION.finditer(output):
        loc_path = m.group(1).replace("../../", "", 1)
        if loc_path.startswith(workdir + "/"):
            loc_path = loc_path[len(workdir) + 1 :]
        if ws_marker in loc_path:
            loc_path = loc_path.split(ws_marker, 1)[1]
        loc_path = os.path.normpath(loc_path).strip("/")
        if loc_path.startswith(("bazel-out/", "external/", "out/")):
            continue
        if os.path.basename(loc_path) in ("BUILD.bazel", "BUILD.gn") or loc_path.endswith(
            (".bzl", ".gni")
        ):
            continue
        if loc_path in changed_set:
            return None
        loc_pkg = _owning_pkg_dir(loc_path)
        in_failing_target_pkg = loc_pkg in t_pkgs or any(
            loc_path == p or loc_path.startswith(p + "/") for p in t_pkgs
        )
        if not in_failing_target_pkg and (loc_pkg in dirs or loc_pkg in changed_pkgs):
            return None
        if loc_path.endswith(src_exts):
            if not _existed_at_change_base(loc_path):
                return None
            if in_failing_target_pkg:
                has_upstream_src_loc = True
    if not has_upstream_src_loc:
        return None
    return (
        f"the failing Bazel target(s) ({' '.join(targets)}) are not built for "
        "this platform in the configured GN graph (<build dir>/ninja_outputs.json) "
        "and fail with pre-existing source compilation errors in untouched source "
        "files. Do not edit source files (`build_only_scope`) or report this "
        "pre-existing failure in `disputed_findings` (set `disputed_findings` to "
        "`[]` in `CoderReport`); the migrated packages are verified by "
        "`shac_check` and the remaining steps."
    )


def _modified_lines_since_base(p):
    lines = set()
    try:
        diff_out = subprocess.check_output(
            ["git", "-C", workdir, "diff", "-U0", change_base, "--", p],
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except Exception:
        return lines
    for m in re.finditer(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", diff_out, re.M):
        start = int(m.group(1))
        count = int(m.group(2)) if m.group(2) is not None else 1
        if count > 0:
            lines.update(range(start, start + count))
        elif start > 0:
            lines.add(start)
    return lines


def _top_level_starlark_spans(p):
    text = read(p)
    if not text:
        return []
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return []
    spans = []
    for stmt in tree.body:
        start_ln = getattr(stmt, "lineno", 0) or 0
        end_ln = getattr(stmt, "end_lineno", start_ln) or start_ln
        if start_ln > 0:
            spans.append((start_ln, max(start_ln, end_ln)))
    return spans


# Check whether a file outside `target_dirs` was introduced or modified by a stacked
# prerequisite commit above `origin/main` and received at most collateral edits in
# the current change.
def _is_collateral_stacked_parent_file(p):
    pkg_d = os.path.dirname(p).strip("/")
    if _path_in_pkg_dirs(p, target_dirs):
        return False
    if _owned_by_stacked_parent_commit(p):
        return True
    if not _existed_at_change_base(p):
        return False
    if os.path.basename(p) == "BUILD.bazel" and _has_substantive_bazel_target_change(pkg_d):
        return False
    mb = (git_lines(["merge-base", "origin/main", "HEAD"]) or [""])[0]
    if mb and git_lines(["log", "--format=%H", f"{mb}..HEAD~1", "--", p]):
        return True
    return False


# Check whether a `bazel_build_fuchsia` or `bazel_build_host` failure is solely a
# package-loading or Starlark evaluation error in an untouched prerequisite package
# from a stacked parent commit above `origin/main`.
_PKG_LOAD_BZL_ERR = re.compile(
    r"error loading package '([^']+)': "
    r"(?:cannot load|Unable to load package for) '([^']+)':"
)
# Starlark traceback frames in a BUILD.bazel file; the captured path is normalized to
# checkout-relative by `_norm_regenerator_workspace_path`.
_STARLARK_TB_BUILD_FILE = re.compile(
    r'File "([^"\s]+/BUILD\.bazel)"'
    r",\s*line\s+\d+"
)


def _stacked_parent_pkg_eval_failure(output, err_lines):
    tb_pkgs = []
    for m in _STARLARK_TB_BUILD_FILE.finditer(output):
        rel_bz = _norm_regenerator_workspace_path(m.group(1))
        pkg_d = os.path.dirname(rel_bz).strip("/")
        if not pkg_d or pkg_d in target_dirs:
            return None
        if not _is_collateral_stacked_parent_file(rel_bz):
            return None
        if pkg_d not in tb_pkgs:
            tb_pkgs.append(pkg_d)
    if not tb_pkgs:
        return None
    for line_txt in err_lines:
        if re.search(
            r"Traceback \(most recent call last\)|"
            r"Analysis of target .* failed; build aborted",
            line_txt,
            re.I,
        ):
            continue
        if any(
            f"package '{dp}'" in line_txt
            or f"'//{dp}:" in line_txt
            or f"/{dp}/BUILD.bazel" in line_txt
            or line_txt.startswith(f"ERROR: {dp}/BUILD.bazel")
            for dp in tb_pkgs
        ):
            continue
        return None
    joined = " ".join(f"{dp}/BUILD.bazel" for dp in tb_pkgs)
    return (
        f"the failing Bazel package evaluation(s) ({joined}) originate from untouched "
        "prerequisite `BUILD.bazel` file(s) in unlanded stacked parent commit(s) above "
        "`origin/main` rather than this change. Do not rewrite unrelated rules in "
        "prerequisite commits here (update the prerequisite CL separately so downstream "
        "cherry-picks apply cleanly), ensure commit `Test:` footers use runnable "
        "verification commands that exit 0 (such as `fx build //<dir>:tests`), "
        "and set `disputed_findings` to `[]` in `CoderReport`."
    )


def unrelated_stacked_parent_bazel_load_failure(step_name, output):
    _ = step_name
    err_lines = [
        l.strip()
        for l in output.splitlines()
        if l.strip().startswith("ERROR:")
        and not l.strip().startswith("ERROR: Build did NOT complete successfully")
    ]
    if not err_lines:
        return None
    eval_note = _stacked_parent_pkg_eval_failure(output, err_lines)
    if eval_note:
        return eval_note
    failing_pkgs = []
    for line_txt in err_lines:
        m = _PKG_LOAD_BZL_ERR.search(line_txt)
        if not m:
            return None
        pkg_d = m.group(1).strip("/")
        bzl_lbl = m.group(2).strip()
        if not pkg_d or pkg_d in target_dirs:
            return None
        bz_rel = f"{pkg_d}/BUILD.bazel"
        if not _is_collateral_stacked_parent_file(bz_rel):
            return None
        try:
            base_src = subprocess.check_output(
                ["git", "-C", workdir, "show", f"{change_base}:{bz_rel}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            return None
        if bzl_lbl not in base_src:
            return None
        if bz_rel not in failing_pkgs:
            failing_pkgs.append(bz_rel)
    if not failing_pkgs:
        return None
    joined = " ".join(failing_pkgs)
    return (
        f"the failing Bazel package load(s) ({joined}) originate from untouched "
        "`load(...)` statements in unlanded stacked prerequisite commit(s) above "
        "`origin/main` rather than this change. Do not rewrite unrelated rules in "
        "prerequisite commits here (update the prerequisite CL separately so "
        "downstream cherry-picks apply cleanly), ensure commit `Test:` footers use "
        "runnable verification commands that exit 0 (such as `fx build //<dir>:tests`), "
        "and set `disputed_findings` to `[]` in `CoderReport`."
    )


def _is_unchanged_preexisting_warning(p, line_num):
    if p not in changed_set or line_num <= 0:
        return False
    if not _existed_at_change_base(p):
        return False
    mod_lines = _modified_lines_since_base(p)
    if not mod_lines:
        return False
    if line_num in mod_lines:
        return False
    if p.endswith((".bazel", ".bzl")):
        for start_ln, end_ln in _top_level_starlark_spans(p):
            if start_ln <= line_num <= end_ln:
                return not any(start_ln <= ln <= end_ln for ln in mod_lines)
    return True


def _path_in_change(p):
    return p in changed_set or _path_in_pkg_dirs(p, dirs)


_GLOB_ALLOWLIST_STALE_ENTRY = re.compile(
    r"^\s*([^\s:;]+)\s+(?:contains no Bazel files that use|doesn't exist|no longer uses)\b"
)


def _shac_finding_in_change(p, msg):
    if _path_in_change(p):
        return True
    if p in ("build/bazel/shac/glob_allowlist.json", "build/bazel/shac/recursive_glob_allowlist.json"):
        m_entry = _GLOB_ALLOWLIST_STALE_ENTRY.match(msg or "")
        if not m_entry:
            return True
        entry = m_entry.group(1)
        if entry.endswith("/"):
            entry_dir = os.path.normpath(entry.rstrip("/"))
            return any(f == entry_dir or f.startswith(entry_dir + "/") for f in changed_set) or any(
                d == entry_dir or d.startswith(entry_dir + "/") or entry_dir.startswith(d + "/")
                for d in dirs
            )
        return _path_in_change(os.path.normpath(entry))
    return False


# Check whether every `shac check` finding is `third_party_readme_update` on build-only
# `third_party/` edits in an uncommitted working tree (where `shac` cannot inspect
# `Skip-Readme-Check:` in a commit message yet, while `build_only_scope` forbids editing
# `README.fuchsia` and `commit_message_format` enforces the footer on commit).
def _build_only_third_party_readme_shac_note(finding_lines):
    if not finding_lines:
        return None
    if not all(
        re.match(r"^\[third_party_readme_update/(?:warning|error)\]", l)
        for l in finding_lines
    ):
        return None
    tp_files = [f for f in changed_set if f.startswith("third_party/")]
    if not tp_files:
        return None
    if not all(
        os.path.basename(f) in BUILD_FILE_NAMES or f.endswith(BUILD_FILE_EXTS)
        for f in tp_files
    ):
        return None
    base_sha = (
        git_lines(["rev-parse", "--verify", f"{change_base}^{{commit}}"]) or [""]
    )[0]
    uncommitted_worktree = bool(
        worktree_dirty_vs_head or not head_sha or head_sha == base_sha
    )
    head_msg = "\n".join(git_lines(["log", "-1", "--format=%B", "HEAD"]))
    has_skip_footer = bool(
        re.search(r"(?m)^Skip-Readme-Check:\s*\S", head_msg)
    )
    if not uncommitted_worktree and not has_skip_footer:
        return None
    return (
        "the only failing shac check is `third_party_readme_update` on build-only "
        "`third_party/` file(s), where `build_only_scope` forbids modifying `README.fuchsia` "
        "and `shac` only reads `Skip-Readme-Check:` from committed messages. Include "
        "`Skip-Readme-Check: No version change, Bazel build files only.` in the commit "
        "message footer block and set `disputed_findings` to `[]` in `CoderReport` "
        "(except for any verified `review_diff` false positives)."
    )


# Check whether a `shac check` failure (such as repo-wide `verify_bazel2gn`, stacked
# ancestor commit messages, or pre-existing `unchanged_lines_warning_only` warnings on
# untouched targets of modified files) only reports items outside the task's change.
def unrelated_shac_failure(output):
    if not dirs or any(
        p.startswith(("scripts/shac/", "build/tools/bazel2gn/")) for p in changed
    ):
        return None
    if re.search(r"^shac:\s*fail:|Traceback \(most recent call last\):", output, re.M):
        return None
    finding_lines = [
        l for l in output.splitlines()
        if re.match(r"^\[[\w_-]+/(?:warning|error)\]", l)
    ]
    if not finding_lines:
        return None
    tp_readme_note = _build_only_third_party_readme_shac_note(finding_lines)
    if tp_readme_note:
        return tp_readme_note
    parsed_findings = []
    for l in finding_lines:
        m = SHAC_FINDING.match(l)
        if not m:
            return None
        if m.group(2) in ("warning", "error"):
            parsed_findings.append((
                m.group(1),
                m.group(2),
                m.group(3),
                m.group(4),
                int(m.group(5) or 0),
                m.group(6) or "",
            ))
    if not parsed_findings:
        return None
    change_base_sha = (git_lines(["rev-parse", "--verify", f"{change_base}^{{commit}}"]) or [""])[0]
    unrelated_items = []
    unchanged_line_warnings = []
    for _, level, commit_sha, raw_path, line_num, msg_text in parsed_findings:
        if commit_sha:
            if (
                head_sha
                and change_base_sha
                and head_sha != change_base_sha
                and not worktree_dirty_vs_head
                and (not uploaded_commit_env or head_sha != uploaded_commit_env)
                and head_sha.startswith(commit_sha)
            ):
                return None
            item = f"Commit {commit_sha}"
            if item not in unrelated_items:
                unrelated_items.append(item)
            continue
        p = os.path.normpath((raw_path or "").replace("../../", "", 1))
        if not p or p.startswith("out/"):
            return None
        in_change = _shac_finding_in_change(p, msg_text)
        if not in_change or _owned_by_stacked_parent_commit(p):
            if p not in unrelated_items:
                unrelated_items.append(p)
            continue
        if (
            level == "warning" or _is_collateral_stacked_parent_file(p)
        ) and _is_unchanged_preexisting_warning(p, line_num):
            loc = f"{p}:{line_num}"
            if loc not in unchanged_line_warnings:
                unchanged_line_warnings.append(loc)
            continue
        return None
    if unrelated_items and not unchanged_line_warnings:
        joined = " ".join(unrelated_items)
        return (
            f"the failing shac check item(s) ({joined}) are outside the changed "
            "packages/commits and were not modified by this change, so the failure "
            "predates the change (e.g. upstream verify_bazel2gn drift or parent commit). "
            "Do not edit unrelated packages or report this pre-existing failure in "
            "`disputed_findings` (set `disputed_findings` to `[]` in `CoderReport` and "
            "omit `fx lint` from `Test:` footers when it exits non-zero); "
            "the changed packages are still verified by the remaining steps."
        )
    if unchanged_line_warnings and not unrelated_items:
        joined = " ".join(unchanged_line_warnings)
        return (
            f"the failing shac check warning(s) ({joined}) are on unchanged lines/targets "
            "of pre-existing file(s) outside the modified hunks (e.g. unchanged_lines_warning_only), "
            "so they predate this change and do not block Gerrit CQ. "
            "Do not edit untouched pre-existing targets; the modified lines and changed packages "
            "are still verified."
        )
    if unrelated_items or unchanged_line_warnings:
        joined = " ".join(unrelated_items + unchanged_line_warnings)
        return (
            f"the failing shac check finding(s) ({joined}) are either outside the changed "
            "packages/commits or warnings on unchanged lines/targets of pre-existing file(s) "
            "outside the modified hunks, so they predate this change. "
            "Do not edit unrelated packages or untouched pre-existing targets; the modified lines "
            "and changed packages are still verified."
        )
    return None


# Check whether every `shac check` finding is a HEAD commit-message finding on planter's own
# single-line `[DO NOT SUBMIT/REVIEW]` banner (longer than shac's 72-column body wrap).
# Planter inserts and strips that banner itself and commit_message_format forbids wrapping or
# editing it, so such findings must not block the coder (who could only delete the banner).
PLANTER_BANNER = (
    "[DO NOT SUBMIT/REVIEW] waiting on uploader acknowledgement of planter findings"
)


def planter_banner_only_shac_failure(output):
    finding_lines = [
        l for l in output.splitlines()
        if re.match(r"^\[[\w_-]+/(?:warning|error)\]", l)
    ]
    if not finding_lines or not head_sha:
        return None
    try:
        raw_msg = subprocess.check_output(
            ["git", "-C", workdir, "log", "-1", "--format=%B", "HEAD"],
            stderr=subprocess.DEVNULL, text=True,
        )
    except Exception:
        return None
    msg_lines = raw_msg.splitlines()
    banner_lns = {i + 1 for i, l in enumerate(msg_lines) if l.rstrip() == PLANTER_BANNER}
    if not banner_lns:
        return None
    for l in finding_lines:
        m = SHAC_FINDING.match(l)
        # Only commit findings on HEAD whose line is the banner line qualify.
        if not m or not m.group(3) or not head_sha.startswith(m.group(3)):
            return None
        if not m.group(5) or int(m.group(5)) not in banner_lns:
            return None
    return (
        "the only failing shac check finding(s) are on planter's own single-line "
        f"`{PLANTER_BANNER}` commit-message banner. Planter adds and strips that banner "
        "itself; leave it on one unmodified line (do not wrap or delete it) and do not "
        "report this as a machinery conflict."
    )


# Probe that .git, the build output directory, and the checkout accept non-empty
# file writes (degraded FUSE/casfs mounts allow 0-byte O_CREAT metadata ops while
# failing write() with ENOENT/EROFS/EIO).
def workspace_write_probe_error():
    probe_bytes = b"planter-write-probe\n"
    probe_dirs = []
    try:
        gdir = subprocess.check_output(
            ["git", "-C", workdir, "rev-parse", "--absolute-git-dir"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
        if gdir and os.path.isdir(gdir):
            probe_dirs.append(gdir)
    except (OSError, subprocess.CalledProcessError):
        pass
    bdir_rel = (read(".fx-build-dir") or "").strip()
    if bdir_rel:
        bdir_abs = os.path.join(workdir, bdir_rel)
        out_parent = os.path.dirname(bdir_abs)
        for cand_dir in (out_parent, bdir_abs):
            if os.path.isdir(cand_dir) and cand_dir not in probe_dirs:
                probe_dirs.append(cand_dir)
    else:
        out_abs = os.path.join(workdir, "out")
        if os.path.isdir(out_abs) and out_abs not in probe_dirs:
            probe_dirs.append(out_abs)
    if os.path.isdir(workdir) and workdir not in probe_dirs:
        probe_dirs.append(workdir)
    for pdir in probe_dirs:
        fd = None
        probe_path = None
        try:
            fd, probe_path = tempfile.mkstemp(
                dir=pdir, prefix=".planter-write-probe."
            )
            os.write(fd, probe_bytes)
            os.close(fd)
            fd = None
            with open(probe_path, "rb") as rf:
                if rf.read() != probe_bytes:
                    raise OSError("short write during writability probe")
            os.unlink(probe_path)
        except OSError as err:
            if fd is not None:
                try:
                    os.close(fd)
                except OSError:
                    pass
            if probe_path and os.path.lexists(probe_path):
                try:
                    os.unlink(probe_path)
                except OSError:
                    pass
            return pdir, err
    return None


# Emit a single workspace_write_error finding when the checkout/build mount rejects writes.
def append_workspace_write_error(pdir, err):
    msg = (
        f"Filesystem write failed during writability probe in `{pdir}` ({err}). "
        "The checkout, `.git`, or build directory mount is rejecting file writes "
        "(for example a degraded FUSE/casfs mount), so `fx build`, `fx lint`, "
        "and `fx bazel` cannot run."
    )
    rem = (
        "This is a workspace filesystem/mount write failure, not a BUILD.bazel or "
        "BUILD.gn defect. Do not edit build files to work around it; stop retrying "
        "failing build or `git commit` write commands, set `test_footers` to "
        "`[\"fx lint\"]` and `disputed_findings` to `[]` in "
        "`CoderReport` (an empty `test_footers` fails `test_verifier` with "
        "`missing_test_footer`, while `fx bazel`/`fx build`/`fx test` target "
        "commands trigger spurious `test_verifier` query errors on an unwritable "
        "mount), and report the mount write error in `summary`."
    )
    findings.append({
        "source": "build_verification",
        "category": "workspace_write_error",
        "severity": "ERROR",
        "message": msg,
        "remediation": rem,
    })


if not dry_run:
    upfront_write_err = workspace_write_probe_error()
    if upfront_write_err is not None:
        # Drop pre-build commit Test: footer errors when .git/mount rejects writes.
        findings[:] = [f for f in findings if f.get("severity") not in STEP_BLOCKING]
        append_workspace_write_error(*upfront_write_err)
        print(json.dumps(findings, indent=2))
        sys.exit(0)

# Return any GN prerequisite targets needed before running `bazel_build_fuchsia`.
def fuchsia_platform_gn_prereqs():
    if not fuchsia_bazel_dirs:
        return []
    toolchains, _ = load_toolchains()
    if toolchains is None:
        return []
    prereqs = []
    for cand in ("//build/bazel:generate_main_workspace",):
        if cand in toolchains:
            prereqs.append(cand)
    return prereqs


failed = set()
step_findings_start = len(findings)
gn_build_scoped = False
for name, cmd in steps:
    if name == "gn_build":
        if os.environ.get("PLANTER_FULL_GN_BUILD", "").strip() not in ("1", "true"):
            labels, notes = dependent_labels()
            for prereq in fuchsia_platform_gn_prereqs():
                if prereq not in labels:
                    labels.append(prereq)
            if labels:
                cmd = [fx, "build"] + labels
                gn_build_scoped = True
                findings.extend(
                    {"source": "build_verification", "category": "dependents_note", "severity": "INFO", "message": n} for n in notes
                )
    if name == "gn_build_dependents":
        if "gn_build" in failed or gn_build_scoped:
            continue
        labels, notes = dependent_labels()
        findings.extend(
            {"source": "build_verification", "category": "dependents_note", "severity": "INFO", "message": n} for n in notes
        )
        if not labels:
            continue
        cmd = [fx, "build"] + labels
    if name == "bazel_tests_exported":
        if dry_run:
            findings.append({
                "source": "build_verification",
                "category": "build_dry_run",
                "severity": "INFO",
                "message": "PLANTER_BUILD_DRY_RUN: bazel_tests_exported would check <build dir>/tests.json for "
                           + " ".join(exported_tests["target_tests"] + exported_tests["host_tests"]),
            })
            continue
        if "gn_build" in failed:
            continue
        findings.extend(
            {"source": "build_verification", "category": "bazel_tests_note", "severity": "INFO", "message": n}
            for n in bazel_tests_export_notes()
        )
        continue
    # Refresh Bazel step commands after `gn_build` so newly generated `tests_json.gn_targets` is used.
    refreshed_cmd = build_bazel_step_cmd(name)
    if refreshed_cmd is not None:
        cmd = refreshed_cmd
    if dry_run:
        findings.append({
            "source": "build_verification",
            "category": "build_dry_run",
            "severity": "INFO",
            "message": f"PLANTER_BUILD_DRY_RUN: {name} would run `{fmt_cmd(cmd)}`",
        })
        continue
    # Refresh `<build dir>/host_*/bazel2gn` before running `shac check` when `//build/tools/bazel2gn`
    # or `bazel-bin/.../bazel2gn` is newer than the Ninja host binary.
    if name == "shac_check" and _bazel2gn_host_tool_stale():
        try:
            subprocess.run(
                [fx, "build", "--host", "//build/tools/bazel2gn:bazel2gn"],
                cwd=workdir,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired:
            pass
    try:
        proc = subprocess.run(cmd, cwd=workdir, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=timeout)
        code, output = proc.returncode, proc.stdout
    except subprocess.TimeoutExpired as e:
        code = "timeout"
        output = (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
        output += f"\n(timed out after {timeout}s)"
    # If target-scoped gn_build ran regenerator (updating ninja_outputs.json) and Ninja
    # rejected a pre-regeneration target that lost its GN consumers (e.g. a library whose
    # sole GN caller became bazel_ffx_tool/bazel_host_tool), refresh dependent_labels()
    # from the newly regenerated ninja_outputs.json and retry once.
    if (
        name == "gn_build"
        and gn_build_scoped
        and code != 0
        and code != "timeout"
        and "ninja: error: unknown target" in output
    ):
        refreshed_labels, refreshed_notes = dependent_labels()
        if refreshed_labels != labels:
            labels = refreshed_labels
            cmd = ([fx, "build"] + labels) if labels else [fx, "build"]
            gn_build_scoped = bool(labels)
            findings[:] = [
                f for f in findings if f.get("category") != "dependents_note"
            ] + [
                {
                    "source": "build_verification",
                    "category": "dependents_note",
                    "severity": "INFO",
                    "message": n,
                }
                for n in refreshed_notes
            ]
            try:
                proc = subprocess.run(
                    cmd,
                    cwd=workdir,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    timeout=timeout,
                )
                code, output = proc.returncode, proc.stdout
            except subprocess.TimeoutExpired as e:
                code = "timeout"
                output = (
                    (e.stdout or b"").decode(errors="replace")
                    if isinstance(e.stdout, bytes)
                    else (e.stdout or "")
                )
                output += f"\n(timed out after {timeout}s)"
    if code == 0:
        continue
    if name == "shac_check":
        output = _FX_LINT_CALLED_PROCESS_TRACEBACK.sub("", output)
    mid_write_err = workspace_write_probe_error()
    if mid_write_err is not None:
        failed.add(name)
        append_workspace_write_error(*mid_write_err)
        break
    if name == "shac_check":
        note = planter_banner_only_shac_failure(output) or unrelated_shac_failure(output)
        if note:
            text, file, line = summarize(output, SHAC_FILE_LOCATION)
            findings.append({
                "source": "build_verification",
                "category": "preexisting_failure_shac_check",
                "severity": "INFO",
                "file": file,
                "line": line,
                "message": (
                    f"`{fmt_cmd(cmd)}` failed ({code}) outside the change: "
                    f"{note}\n{text}"
                ),
            })
            continue
    if name == "gn_build_dependents" and code != "timeout":
        own_only = [
            lbl
            for lbl in labels
            if any(
                lbl.split("(", 1)[0].split(":", 1)[0] == f"//{d}"
                for d in dirs
            )
        ]
        if own_only and own_only != labels and unrelated_bazel_failure(output):
            own_cmd = [fx, "build"] + own_only
            try:
                own_proc = subprocess.run(
                    own_cmd,
                    cwd=workdir,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                    timeout=timeout,
                )
                if own_proc.returncode == 0:
                    continue
            except subprocess.TimeoutExpired:
                pass
    if name in ("bazel_build_fuchsia", "bazel_build_host"):
        ext_note = external_repo_bazel_failure(output)
        if ext_note:
            text, file, line = summarize(output, LOCATION)
            findings.append({
                "source": "build_verification",
                "category": f"preexisting_failure_{name}",
                "severity": "INFO",
                "file": file,
                "line": line,
                "message": (
                    f"`{fmt_cmd(cmd)}` failed ({code}) in external repository dependency: "
                    f"{ext_note}\n{text}"
                ),
            })
            continue
    if name in ("gn_build", "gn_build_dependents"):
        note = unrelated_bazel_failure(output)
        if note:
            text, file, line = summarize(output, LOCATION)
            findings.append({
                "source": "build_verification",
                "category": f"preexisting_failure_{name}",
                "severity": "INFO",
                "file": file,
                "line": line,
                "message": (
                    f"`{fmt_cmd(cmd)}` failed ({code}) outside the change: {note}\n{text}"
                ),
            })
            continue
    if name in ("bazel_build_fuchsia", "bazel_build_host") and code != "timeout":
        note = unrelated_upstream_bazel_compile_failure(
            name, output
        ) or unrelated_stacked_parent_bazel_load_failure(name, output)
        if note:
            text, file, line = summarize(output, LOCATION)
            findings.append({
                "source": "build_verification",
                "category": f"preexisting_failure_{name}",
                "severity": "INFO",
                "file": file,
                "line": line,
                "message": (
                    f"`{fmt_cmd(cmd)}` failed ({code}) outside the change: {note}\n{text}"
                ),
            })
            continue
    failed.add(name)
    loc_re = (
        SHAC_FILE_LOCATION
        if name == "shac_check"
        else (BUILD_FILE_LOCATION if name == "gn_build_dependents" else LOCATION)
    )
    text, file, line = summarize(output, loc_re)
    step_remediation = (
        wired_suite_external_repo_remediation(name, output) or REMEDIATION[name]
    )
    findings.append({
        "source": "build_verification",
        "category": f"build_failure_{name}",
        "severity": "ERROR",
        "file": file,
        "line": line,
        "message": f"`{fmt_cmd(cmd)}` failed ({code}):\n{text}",
        "remediation": step_remediation,
    })
    if name == "gn_build" and re.search(r"fx set|no build directory|Unable to find build directory", output, re.I):
        break  # The tree is not configured; the remaining steps would fail the same way.

step_findings = findings[step_findings_start:]
check_ext_repo_failed_test_footers(step_findings, locals().get("test_footer_lines", []), dirs)
if reuse_key and not any(f.get("severity") in STEP_BLOCKING for f in step_findings):
    try:
        if _fingerprint() != reuse_key:
            raise OSError("the change was edited during the build")
        entries = {}
        try:
            with open(reuse_path) as f:
                old = json.load(f)
            if isinstance(old.get("entries"), dict):
                entries = old["entries"]
            elif old.get("key"):
                entries[old["key"]] = {"time": old.get("time", 0), "findings": old.get("findings", [])}
        except (OSError, ValueError):
            pass
        now = time.time()
        entries[reuse_key] = {"time": now, "findings": step_findings}
        if len(entries) > 20:
            newest = sorted(entries.items(), key=lambda kv: kv[1].get("time", 0), reverse=True)[:20]
            entries = dict(newest)
        fd, tmp = tempfile.mkstemp(dir=os.path.dirname(reuse_path), prefix=".planter-build-verification.")
        with os.fdopen(fd, "w") as f:
            json.dump({"key": reuse_key, "time": now, "findings": step_findings, "entries": entries}, f)
        os.replace(tmp, reuse_path)
    except (OSError, ValueError, subprocess.CalledProcessError):
        pass

print(json.dumps(findings, indent=2))
PYEOF
