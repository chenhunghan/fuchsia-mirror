#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared helpers for Planter deterministic check scripts and tools."""

import ast
import fcntl
import json
import os
import re
import signal
import subprocess
import time

EXCLUDED_GLOBAL_DIRS = {
    "bundles/assembly",
    "build/bazel",
    "build/bazel2gn",
    "build/images",
    "build/config/rust/lints",
    "third_party/rust_crates",
}

BAZEL_TEST_RULES = {
    "cc_test",
    "fx_cc_test_binary",
    "fx_test",
    "fx_test_component",
    "fx_unittest_component",
    "fx_unittest_package",
    "go_test",
    "golden_test",
    "host_cc_test",
    "host_go_test",
    "host_py_test",
    "host_rustc_test",
    "host_shac_test",
    "host_test",
    "host_test_data_files",
    "rust_test",
    "rustc_test",
    "wrap_host_rust_test",
}

GN_NON_TARGET_KEYWORDS = {
    "assert",
    "declare_args",
    "else",
    "foreach",
    "forward_variables_from",
    "if",
    "import",
    "pool",
    "set_default_toolchain",
    "set_defaults",
    "template",
    "toolchain",
}

NOT_TARGETS = GN_NON_TARGET_KEYWORDS

# Starlark rule and macro keyword attributes whose string values are path sentinels,
# compiler/linker/CLI flags, visibility/license specs, target metadata, or bazel2gn
# GN-conversion-only pass-through strings rather than Bazel target dependency labels.
BAZEL_NON_DEP_LABEL_ATTRS = frozenset(
    {
        "additional_cpp_configs",
        "api_area",
        "applicable_licenses",
        "args",
        "binary_name",
        "category",
        "cmd",
        "cmd_bash",
        "component_name",
        "configs",
        "conlyopts",
        "copts",
        "crate_name",
        "cxxopts",
        "default_applicable_licenses",
        "default_visibility",
        "defines",
        "depfile",
        "env",
        "fidl_library_name",
        "friend",
        "gn_output_name",
        "idk_name",
        "imports",
        "include_base",
        "include_dir",
        "includes",
        "inputs",
        "install_root",
        "ldflags",
        "library_name",
        "linkopts",
        "local_defines",
        "max_log_severity",
        "mnemonic",
        "name",
        "non_fidl_deps",
        "output_name",
        "package_name",
        "packages",
        "public_configs",
        "restricted_features",
        "rustc_env",
        "rustc_flags",
        "stub_name",
        "tags",
        "test_args",
        "test_rustc_flags",
        "test_type",
        "tool_output_name",
        "visibility",
    }
)


# Collect AST node IDs inside non-dependency keyword attributes or package_group calls.
def collect_non_dep_ast_node_ids(tree, extra_attrs=()):
    skip_attrs = (
        BAZEL_NON_DEP_LABEL_ATTRS | frozenset(extra_attrs)
        if extra_attrs
        else BAZEL_NON_DEP_LABEL_ATTRS
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
                if kw.arg in skip_attrs:
                    for sub in ast.walk(kw.value):
                        node_ids.add(id(sub))
    return node_ids


SENTINEL_RE = re.compile(
    r"^[ \t]*(?:##\s*BAZEL2GN SENTINEL[^\n]*|#LOCAL_BAZEL_BUILD_SENTINEL[^\n]*)",
    re.MULTILINE,
)

_GN_CALL_RE = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"$]+)"\s*\)\s*\{')
_WARNING_SEVERITIES = {"WARNING", "WARN", "MINOR", "MEDIUM", "MODERATE"}
_INFO_SEVERITIES = {
    "INFO",
    "INFORMATIONAL",
    "NOTE",
    "NIT",
    "NITPICK",
    "SUGGESTION",
    "OPTIONAL",
    "LOW",
    "FYI",
}


def normalize_severity(severity):
    s = (severity or "").strip().upper()
    if s in _WARNING_SEVERITIES:
        return "WARNING"
    if s in _INFO_SEVERITIES:
        return "INFO"
    return "ERROR"


def emit(
    findings,
    category,
    severity,
    file="",
    line=0,
    message="",
    remediation="",
    source=None,
):
    """Appends a standardized finding dictionary to `findings` and returns it."""
    src = (
        source
        or os.environ.get("PLANTER_CHECK_NAME", "").strip()
        or "planter_check"
    )
    raw_sev = (severity or "").strip()
    sev_out = (
        raw_sev
        if raw_sev in ("error", "warning", "info", "ERROR", "WARNING", "INFO")
        else normalize_severity(raw_sev)
    )
    entry = {
        "source": src,
        "category": category or "",
        "severity": sev_out,
        "file": file or "",
        "line": int(line or 0),
        "message": message or "",
        "remediation": remediation or "",
    }
    if findings is not None:
        findings.append(entry)
    return entry


def emit_json(findings):
    """Prints `findings` as formatted JSON to stdout."""
    print(json.dumps(findings, indent=2))


def normalize_target_dir(raw_dir, workdir=None):
    """Normalizes a package directory argument to a checkout-relative path."""
    d = (raw_dir or "").strip()
    if d.startswith("//"):
        d = d[2:]
    elif workdir and os.path.isabs(d):
        rel = os.path.relpath(os.path.abspath(d), os.path.abspath(workdir))
        if rel != ".." and not rel.startswith("../"):
            d = rel
    while d.startswith("./"):
        d = d[2:]
    d = d.strip("/")
    return "" if d == "." else d


# Parse whitespace- or comma-delimited target directory arguments into normalized
# checkout-relative paths while preserving order.
def parse_target_dirs(raw_dirs, workdir=None):
    """Splits whitespace- or comma-separated target dirs (or an iterable of dirs) into normalized unique paths."""
    raw_items = (
        re.split(r"[\s,]+", raw_dirs or "")
        if raw_dirs is None or isinstance(raw_dirs, str)
        else list(raw_dirs)
    )
    out = []
    for item in raw_items:
        norm = normalize_target_dir(item, workdir=workdir)
        if norm and norm not in out:
            out.append(norm)
    return out


def _git_out(workdir, args):
    try:
        return subprocess.check_output(
            ["git", "-C", workdir] + list(args),
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        return ""


def _is_ancestor(workdir, anc, desc):
    if not anc or not desc:
        return False
    try:
        return (
            subprocess.run(
                [
                    "git",
                    "-C",
                    workdir,
                    "merge-base",
                    "--is-ancestor",
                    anc,
                    desc,
                ],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            ).returncode
            == 0
        )
    except Exception:
        return False


def with_overlaid_target_dirs(tdirs):
    """Appends `<dir>` for each GN-overlay `build/secondary/<dir>` target dir.

    A `build/secondary/<dir>` task migrates its code to `//<dir>`, so its commit
    subject, `Bazel-Migration-Target:` footer, and committed files name `<dir>`.
    """
    out = list(tdirs or [])
    for td in list(out):
        if td.startswith("build/secondary/"):
            stripped = td[len("build/secondary/") :]
            if stripped and stripped not in out:
                out.append(stripped)
    return out


# Cache upstream directory rename lookups keyed by (workdir, HEAD SHA, old_dir).
_relocated_target_dir_cache = {}


# Follow git history renames when an assigned target directory was moved upstream.
def resolve_relocated_target_dir(workdir, td):
    """Returns the relocated checkout-relative package path if `td` moved upstream, else ''."""
    cur = normalize_target_dir(td, workdir=workdir)
    if (
        not workdir
        or not cur
        or not os.path.isdir(workdir)
        or cur.startswith("build/secondary/")
        or os.path.exists(os.path.join(workdir, cur))
    ):
        return ""
    abs_wdir = os.path.abspath(workdir)
    head_sha = _git_out(
        workdir, ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]
    )
    cache_key = (abs_wdir, head_sha, cur)
    if cache_key in _relocated_target_dir_cache:
        return _relocated_target_dir_cache[cache_key]
    resolved = ""
    probe = cur
    for _ in range(3):
        del_sha = _git_out(
            workdir,
            [
                "log",
                "-n",
                "1",
                "--format=%H",
                "--diff-filter=D",
                "HEAD",
                "--",
                f"{probe}/BUILD.gn",
                f"{probe}/BUILD.bazel",
            ],
        )
        if not del_sha:
            break
        ren_out = _git_out(
            workdir,
            [
                "-c",
                "diff.renameLimit=5000",
                "diff-tree",
                "-r",
                "-M",
                "--diff-filter=R",
                "--name-status",
                f"{del_sha}^",
                del_sha,
            ],
        )
        exact_cand = ""
        fallback_cand = ""
        for line in ren_out.splitlines():
            parts = line.split("\t")
            if len(parts) < 3 or not parts[0].startswith("R"):
                continue
            old_p = parts[1].strip("/")
            new_p = parts[2].strip("/")
            if old_p in (f"{probe}/BUILD.gn", f"{probe}/BUILD.bazel") and (
                os.path.basename(new_p) in ("BUILD.gn", "BUILD.bazel")
            ):
                exact_cand = os.path.dirname(new_p).strip("/")
                break
            if not fallback_cand and old_p.startswith(probe + "/"):
                rel_sub = old_p[len(probe) + 1 :]
                if rel_sub and new_p.endswith("/" + rel_sub):
                    fallback_cand = new_p[: -(len(rel_sub) + 1)].strip("/")
        next_dir = exact_cand or fallback_cand
        if not next_dir or next_dir == probe:
            break
        probe = next_dir
        if os.path.isdir(os.path.join(workdir, probe)):
            resolved = probe
            break
    _relocated_target_dir_cache[cache_key] = resolved
    return resolved


# Expand target directories with both `build/secondary/` overlays and upstream git renames.
def with_relocated_target_dirs(workdir, tdirs):
    """Appends overlaid and upstream-relocated target directories to `tdirs`."""
    out = with_overlaid_target_dirs(tdirs)
    if not workdir or not os.path.isdir(workdir):
        return out
    for td in list(out):
        reloc = resolve_relocated_target_dir(workdir, td)
        if reloc and reloc not in out:
            out.append(reloc)
    return out


def _resolve_active_target_dirs(workdir, target_dirs=None):
    """Normalizes explicit `target_dirs` or `$PLANTER_TARGET_DIRS` / `$PLANTER_TARGET_DIR`."""
    if target_dirs is None:
        env_wdir = os.environ.get("PLANTER_WORKDIR", "").strip()
        if (
            workdir
            and env_wdir
            and os.path.isdir(env_wdir)
            and os.path.realpath(workdir) != os.path.realpath(env_wdir)
        ):
            return []
        raw_dirs = os.environ.get("PLANTER_TARGET_DIRS", "").strip()
        raw_dir = os.environ.get("PLANTER_TARGET_DIR", "").strip()
        target_dirs = f"{raw_dirs} {raw_dir}".strip()
    parsed = parse_target_dirs(target_dirs, workdir)
    return with_relocated_target_dirs(workdir, parsed)


_SHARED_INFRA_PREFIXES = (
    "build/bazel/rules/",
    "build/bazel/aspects/",
    "build/bazel/toolchains/",
    "build/bazel/starlark/",
    "build/bazel/scripts/",
    "tools/devshell/",
)


# Check whether the git working tree has uncommitted tracked or untracked task files.
def has_uncommitted_worktree_edits(workdir):
    """Returns True when `workdir` has tracked diffs or untracked non-artifact files."""
    if not workdir or not os.path.isdir(workdir):
        return False
    raw = []
    for args in (
        ["diff", "--name-only", "HEAD"],
        ["diff", "--cached", "--name-only"],
        ["ls-files", "--others", "--exclude-standard"],
    ):
        raw.extend(_git_out(workdir, args).splitlines())
    for entry in raw:
        path = entry.strip()
        if not path:
            continue
        if (
            path.startswith("third_party/bazel_vendor/")
            or "/__pycache__/" in path
            or path.startswith("__pycache__/")
            or path.endswith(".pyc")
            or (
                "/" not in path
                and re.fullmatch(r"CoderReport[\w.-]*\.json", path)
            )
        ):
            continue
        return True
    return False


# Match a target directory against a `//<sdir>` commit-subject path token.
def _matches_subject_dir_token(pkg, sdir):
    if not pkg or not sdir:
        return False
    if sdir.startswith(".../"):
        suf = sdir[4:]
        return bool(suf) and (
            pkg == suf or pkg.endswith("/" + suf) or f"/{suf}/" in f"/{pkg}/"
        )
    return (
        pkg == sdir or pkg.startswith(sdir + "/") or sdir.startswith(pkg + "/")
    )


# Detect when HEAD is a stacked prerequisite commit rather than a task commit for `tdirs`.
def is_stacked_prerequisite_head(head_subj, head_msg, head_files, tdirs):
    """Returns True when HEAD is a prerequisite/infrastructure commit outside `tdirs`."""
    if not tdirs:
        return False
    # A build/secondary/<dir> task migrates to //<dir> (commit_message_format.sh), so also
    # match commit subjects and Bazel-Migration-Target footers against the overlaid <dir>.
    tdirs = with_overlaid_target_dirs(tdirs)
    subj = (head_subj or "").strip()
    sdir = (
        subj.split(" //", 1)[1]
        .split()[0]
        .split(":", 1)[0]
        .strip()
        .rstrip(".,;:")
        .strip("/")
        if " //" in subj
        else ""
    )
    subj_matches = bool(sdir) and any(
        _matches_subject_dir_token(td, sdir) for td in tdirs
    )
    if sdir and not subj_matches:
        return True
    bmt_pkgs = {
        m.strip("/")
        for m in re.findall(
            r"(?mi)^Bazel-Migration-Target:\s*//([^:\s]+)", head_msg or ""
        )
        if m.strip("/")
    }
    if bmt_pkgs and not any(
        bp == td or bp.startswith(td + "/") or td.startswith(bp + "/")
        for bp in bmt_pkgs
        for td in tdirs
    ):
        return True
    if not subj.startswith("[bazel_migration]") and not subj_matches:
        touches_shared_infra = any(
            f.strip("/").startswith(_SHARED_INFRA_PREFIXES) for f in head_files
        ) and not any(
            td.startswith(_SHARED_INFRA_PREFIXES)
            or any(p.startswith(td + "/") for p in _SHARED_INFRA_PREFIXES)
            for td in tdirs
        )
        has_unrelated_bmt = any(
            not any(
                bp == td or bp.startswith(td + "/") or td.startswith(bp + "/")
                for td in tdirs
            )
            for bp in bmt_pkgs
        )
        if touches_shared_infra or has_unrelated_bmt:
            return True
    return False


# Check whether a changed path belongs directly to `td` rather than a nested subpackage.
def _is_direct_target_pkg_file(workdir, rel_file, td, commit_files=()):
    """Returns True if `rel_file` belongs to `td` rather than a nested build subpackage."""
    if rel_file == td:
        return True
    if not rel_file.startswith(td + "/"):
        return False
    sub_rel = rel_file[len(td) + 1 :]
    if "/" not in sub_rel:
        return True
    commit_set = set(commit_files)
    parts = sub_rel.split("/")[:-1]
    for idx in range(1, len(parts) + 1):
        sub_pkg = td + "/" + "/".join(parts[:idx])
        for bname in ("BUILD.bazel", "BUILD", "BUILD.gn"):
            cand = f"{sub_pkg}/{bname}"
            if cand in commit_set or os.path.isfile(
                os.path.join(workdir, cand)
            ):
                return False
    return True


def _matches_migration_subject_pkg(pkg, sdir):
    """Returns True if package `pkg` matches the `//<sdir>` target in a commit subject."""
    if not pkg or not sdir:
        return False
    if sdir.startswith(".../"):
        suf = sdir[4:]
        return bool(suf) and (pkg == suf or pkg.endswith("/" + suf))
    return pkg == sdir


# Detect stacked prerequisite commits migrating child or upstream packages outside `tdirs`.
def _is_prerequisite_stack_commit(
    workdir, commit_msg, subj, commit_files, tdirs, check_untracked=False
):
    """Returns True when a migration commit belongs to a stacked prerequisite package."""
    if not tdirs:
        return False
    tdir_set = set(tdirs)
    commit_bazel_pkgs = {
        os.path.dirname(f)
        for f in commit_files
        if os.path.basename(f) in ("BUILD.bazel", "BUILD")
    }
    if commit_bazel_pkgs & tdir_set:
        return False
    if check_untracked:
        untracked = [
            ln.strip()
            for ln in _git_out(
                workdir, ["ls-files", "--others", "--exclude-standard"]
            ).splitlines()
            if ln.strip()
        ]
        untracked_bazel_pkgs = {
            os.path.dirname(f)
            for f in untracked
            if os.path.basename(f) in ("BUILD.bazel", "BUILD")
        }
        if untracked_bazel_pkgs & tdir_set:
            return True
    bmt_pkgs = {
        m.group(1).strip("/")
        for m in re.finditer(
            r"(?mi)^Bazel-Migration-Target\s*:\s*@@?//([^:\s]+)", commit_msg
        )
        if m.group(1).strip("/")
    }
    if bmt_pkgs and not (bmt_pkgs & tdir_set):
        return True
    sdir = (
        subj.split(" //", 1)[1]
        .split()[0]
        .split(":", 1)[0]
        .strip()
        .rstrip(".,;:")
        .strip("/")
        if " //" in subj
        else ""
    )
    if (
        sdir
        and commit_bazel_pkgs
        and not any(_matches_migration_subject_pkg(td, sdir) for td in tdirs)
    ):
        return True
    return False


# Resolve the repository `.git` directory without spawning a git subprocess on the fast path.
def _resolve_git_dir_fast(workdir):
    if not workdir or not os.path.isdir(workdir):
        return ""
    dot_git = os.path.join(workdir, ".git")
    if os.path.isdir(dot_git):
        return dot_git
    if os.path.isfile(dot_git):
        try:
            with open(dot_git, encoding="utf-8") as fh:
                first = fh.readline().strip()
            if first.startswith("gitdir:"):
                raw = first.split(":", 1)[1].strip()
                return (
                    raw
                    if os.path.isabs(raw)
                    else os.path.abspath(os.path.join(workdir, raw))
                )
        except OSError:
            pass
    return ""


# Wait for any active `tests_json_parity` baseline working-tree reversion to restore files
# (or run journal recovery if an interrupted run left `restore.json` behind) so concurrent
# static checks never inspect a temporarily reverted checkout.
def wait_for_tests_json_parity_restore(workdir, timeout_sec=180):
    if os.environ.get("PLANTER_IN_TESTS_JSON_PARITY_RESTORE") == "1":
        return
    gdir = _resolve_git_dir_fast(workdir)
    if not gdir:
        return
    parity_dir = os.path.join(gdir, "planter-tests-json-parity")
    journal_path = os.path.join(parity_dir, "restore.json")
    if not os.path.exists(journal_path):
        return
    lock_path = os.path.join(parity_dir, "lock")
    deadline = time.monotonic() + timeout_sec
    acquired_fd = None
    while time.monotonic() < deadline:
        if not os.path.exists(journal_path):
            return
        try:
            fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o644)
        except OSError:
            return
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            acquired_fd = fd
            break
        except BlockingIOError:
            os.close(fd)
            time.sleep(0.25)
    if acquired_fd is not None:
        still_reverted = os.path.exists(journal_path)
        try:
            fcntl.flock(acquired_fd, fcntl.LOCK_UN)
        except OSError:
            pass
        os.close(acquired_fd)
        if still_reverted:
            parity_sh = os.path.join(
                os.path.dirname(os.path.abspath(__file__)),
                "tests_json_parity.sh",
            )
            if os.path.isfile(parity_sh):
                env = dict(
                    os.environ,
                    PLANTER_WORKDIR=os.path.abspath(workdir),
                    PLANTER_SKIP_BUILD="1",
                    PLANTER_IN_TESTS_JSON_PARITY_RESTORE="1",
                )
                subprocess.run(
                    ["bash", parity_sh],
                    cwd=workdir,
                    env=env,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=30,
                    check=False,
                )


def resolve_change_base(workdir, change_base=None, target_dirs=None):
    """Deterministically resolves the git change base for the current check run."""
    wait_for_tests_json_parity_restore(workdir)
    env_resolved = os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
    raw = (
        change_base
        if change_base is not None
        else (env_resolved or os.environ.get("PLANTER_CHANGE_BASE", ""))
    )
    base = (raw or "").strip() or "HEAD"
    if (
        env_resolved
        and change_base is not None
        and base
        in (
            "HEAD",
            os.environ.get("PLANTER_CHANGE_BASE", "").strip(),
        )
    ):
        base = env_resolved

    m_jj = re.fullmatch(r"@(-*)", base)
    if m_jj:
        base = f"HEAD~{len(m_jj.group(1))}" if m_jj.group(1) else "HEAD"

    if not workdir or not os.path.isdir(workdir):
        return base

    head_sha = _git_out(
        workdir, ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]
    )
    if not head_sha:
        return base

    parent_sha = _git_out(
        workdir, ["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"]
    )
    base_sha = _git_out(
        workdir, ["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"]
    )
    origin_main = _git_out(
        workdir, ["rev-parse", "--verify", "--quiet", "origin/main^{commit}"]
    )
    merge_base = (
        _git_out(workdir, ["merge-base", origin_main, head_sha])
        if (origin_main and head_sha)
        else ""
    )
    head_on_origin = bool(
        origin_main
        and head_sha
        and (head_sha == origin_main or merge_base == head_sha)
    )
    tdirs = _resolve_active_target_dirs(workdir, target_dirs)
    subj = _git_out(workdir, ["log", "-1", "--format=%s", "HEAD"])
    head_msg = _git_out(workdir, ["log", "-1", "--format=%B", "HEAD"])
    head_files = (
        [
            line.strip()
            for line in _git_out(
                workdir, ["diff", "--name-only", "HEAD~1", "HEAD"]
            ).splitlines()
            if line.strip()
        ]
        if parent_sha
        else []
    )
    head_is_prereq = bool(
        parent_sha
        and not head_on_origin
        and _is_prerequisite_stack_commit(
            workdir, head_msg, subj, head_files, tdirs, check_untracked=True
        )
    )

    if (
        base_sha
        and merge_base
        and base_sha != merge_base
        and _is_ancestor(workdir, base_sha, merge_base)
    ):
        base = merge_base
        base_sha = merge_base
    elif (
        head_sha and base_sha and not _is_ancestor(workdir, base_sha, head_sha)
    ):
        if head_is_prereq:
            base = "HEAD"
        else:
            pair_mb = _git_out(workdir, ["merge-base", base_sha, head_sha])
            parent_on_origin = bool(
                origin_main
                and parent_sha
                and _is_ancestor(workdir, parent_sha, origin_main)
            )
            parent_is_prereq = False
            if parent_sha and not parent_on_origin:
                p_subj = _git_out(
                    workdir, ["log", "-1", "--format=%s", "HEAD~1"]
                )
                p_msg = _git_out(
                    workdir, ["log", "-1", "--format=%B", "HEAD~1"]
                )
                p_parent = _git_out(
                    workdir,
                    ["rev-parse", "--verify", "--quiet", "HEAD~2^{commit}"],
                )
                p_files = (
                    [
                        ln.strip()
                        for ln in _git_out(
                            workdir, ["diff", "--name-only", p_parent, "HEAD~1"]
                        ).splitlines()
                        if ln.strip()
                    ]
                    if p_parent
                    else []
                )
                parent_is_prereq = _is_prerequisite_stack_commit(
                    workdir,
                    p_msg,
                    p_subj,
                    p_files,
                    tdirs,
                    check_untracked=False,
                )
            if pair_mb and (
                not merge_base
                or (
                    pair_mb != merge_base
                    and _is_ancestor(workdir, merge_base, pair_mb)
                )
            ):
                base = pair_mb
            elif parent_is_prereq:
                base = "HEAD~1"
            else:
                base = merge_base or ("HEAD~1" if parent_sha else base)
        base_sha = _git_out(
            workdir, ["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"]
        )

    # Do not step back to HEAD~1 when HEAD is already merged on origin/main
    # (e.g. an upstream [bazel_migration] commit at checkout base while edits
    # are uncommitted) or is a stacked prerequisite commit outside target_dirs.
    if parent_sha and not head_on_origin:
        has_task_footer = bool(
            re.search(
                r"^(?:TAG\s*[:=]\s*.*\bplanter\b|Bazel-Migration-Target\s*:)",
                head_msg,
                re.M | re.I,
            )
        )
        is_mig_subj = (
            subj.startswith(("[bazel_migration]", "[bazel2gn]", "[bazel]"))
            or has_task_footer
        )
        tdirs = with_overlaid_target_dirs(
            _resolve_active_target_dirs(workdir, target_dirs)
        )
        # Include submodule gitlink edits hidden by `.gitmodules` `ignore = all`.
        head_files = [
            line.strip()
            for line in _git_out(
                workdir,
                [
                    "diff",
                    "--ignore-submodules=none",
                    "--name-only",
                    "HEAD~1",
                    "HEAD",
                ],
            ).splitlines()
            if line.strip()
        ]
        head_touches_tdirs = any(
            _is_direct_target_pkg_file(workdir, f, td, head_files)
            for td in tdirs
            for f in head_files
        )
        # HEAD is not on origin/main here, so a HEAD commit touching target_dirs
        # is a local task commit unless uncommitted edits sit on top of HEAD or
        # HEAD is a stacked prerequisite commit.
        is_prereq_head = head_is_prereq or is_stacked_prerequisite_head(
            subj, head_msg, head_files, tdirs
        )
        # Advance past a stacked prerequisite commit cherry-picked on top of a pinned base
        # while the current task's edits remain in the working tree (or yet to be made).
        if (
            is_prereq_head
            and base_sha
            and base_sha != head_sha
            and _is_ancestor(workdir, base_sha, head_sha)
        ):
            has_prereq_markers = " //" in subj or bool(
                re.search(r"(?mi)^Bazel-Migration-Target:", head_msg)
            )
            if (
                has_uncommitted_worktree_edits(workdir)
                or not head_touches_tdirs
                or has_prereq_markers
            ):
                return "HEAD"
        is_local_task_head = not is_prereq_head and (
            is_mig_subj or (head_touches_tdirs and bool(origin_main))
        )
        # Keep change_base at HEAD when tdirs is non-empty and HEAD touches none
        # of them (including when tdirs were moved or deleted upstream and HEAD
        # is a stacked prerequisite commit for a different package), or when HEAD
        # already carries a merged Gerrit Reviewed-on: trailer.
        if re.search(r"(?m)^Reviewed-on:\s*\S", head_msg):
            return base
        if is_local_task_head and (base == "HEAD" or base_sha == head_sha):
            if has_uncommitted_worktree_edits(workdir):
                return base
            if not tdirs or head_touches_tdirs:
                return "HEAD~1"
            return base
        if (
            is_local_task_head
            and (head_touches_tdirs or not head_files)
            and base_sha
            and base_sha != parent_sha
            and _is_ancestor(workdir, base_sha, parent_sha)
        ):
            head_cids = re.findall(r"(?mi)^Change-Id:\s*(I[0-9a-f]+)", head_msg)
            parent_subj = _git_out(
                workdir, ["log", "-1", "--format=%s", "HEAD~1"]
            )
            parent_msg = _git_out(
                workdir, ["log", "-1", "--format=%B", "HEAD~1"]
            )
            parent_cids = re.findall(
                r"(?mi)^Change-Id:\s*(I[0-9a-f]+)", parent_msg
            )
            if (
                head_cids and parent_cids and head_cids[-1] != parent_cids[-1]
            ) or (parent_cids and not head_cids):
                return "HEAD~1"
            stack_out = _git_out(
                workdir, ["diff", "--name-only", base_sha, "HEAD~1"]
            )
            stack_files = [
                ln.strip() for ln in stack_out.splitlines() if ln.strip()
            ]
            if is_stacked_prerequisite_head(
                parent_subj, parent_msg, stack_files, tdirs
            ):
                return "HEAD~1"
            if not any(
                any(
                    _is_direct_target_pkg_file(workdir, p, td, stack_files)
                    for td in tdirs
                )
                for p in stack_files
            ):
                return "HEAD~1"
        # Advance past stacked prerequisite commits when edits in target_dirs
        # live in the working tree on top of a prerequisite HEAD commit.
        if (
            is_mig_subj
            and tdirs
            and not head_touches_tdirs
            and (
                base == "HEAD"
                or (
                    base_sha
                    and base_sha != head_sha
                    and _is_ancestor(workdir, base_sha, head_sha)
                )
            )
        ):
            wt_candidates = (
                _git_out(workdir, ["diff", "--name-only", "HEAD"]).splitlines()
                + _git_out(
                    workdir, ["diff", "--cached", "--name-only"]
                ).splitlines()
                + _git_out(
                    workdir, ["ls-files", "--others", "--exclude-standard"]
                ).splitlines()
            )
            wt_touches_tdirs = any(
                f.strip() == td or f.strip().startswith(td + "/")
                for td in tdirs
                for f in wt_candidates
                if f.strip()
            )
            if wt_touches_tdirs:
                stack_out = (
                    _git_out(workdir, ["diff", "--name-only", base_sha, "HEAD"])
                    if base_sha
                    else ""
                )
                stack_files = [
                    ln.strip() for ln in stack_out.splitlines() if ln.strip()
                ]
                if not any(
                    any(p == td or p.startswith(td + "/") for td in tdirs)
                    for p in stack_files
                ):
                    return "HEAD"

    return base


def changed_files(workdir, change_base=None, target_dirs=None):
    """Returns the list of changed and untracked files relative to `change_base`."""
    eff_base = (
        resolve_change_base(workdir, change_base, target_dirs=target_dirs)
        if (workdir and os.path.isdir(workdir))
        else (change_base or "HEAD")
    )
    pinned_base = (
        os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
        or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
    )
    pinned_matches_eff = bool(
        not pinned_base
        or eff_base == pinned_base
        or (
            workdir
            and os.path.isdir(workdir)
            and _git_out(
                workdir,
                ["rev-parse", "--verify", "--quiet", f"{eff_base}^{{commit}}"],
            )
            == _git_out(
                workdir,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    f"{pinned_base}^{{commit}}",
                ],
            )
        )
    )
    # Filter out unmodified stacked prerequisite files outside target_dirs when eff_base is HEAD.
    tdirs = (
        _resolve_active_target_dirs(workdir, target_dirs)
        if (workdir and os.path.isdir(workdir))
        else []
    )
    if (
        "PLANTER_CHANGED_FILES" in os.environ
        and pinned_matches_eff
        and (
            change_base is None
            or change_base
            in (
                "HEAD",
                os.environ.get("PLANTER_CHANGE_BASE", "").strip(),
                os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip(),
                eff_base,
            )
        )
    ):
        env_files = [
            line.strip()
            for line in os.environ["PLANTER_CHANGED_FILES"].splitlines()
            if line.strip()
        ]
        if eff_base == "HEAD" and tdirs and workdir and os.path.isdir(workdir):
            head_files = [
                line.strip()
                for line in _git_out(
                    workdir, ["diff", "--name-only", "HEAD~1", "HEAD"]
                ).splitlines()
                if line.strip()
            ]
            head_touches_tdirs = any(
                f == td or f.startswith(td + "/")
                for td in tdirs
                for f in head_files
            )
            if head_files and not head_touches_tdirs:
                wt_dirty = set(
                    line.strip()
                    for line in _git_out(
                        workdir, ["diff", "--name-only", "HEAD"]
                    ).splitlines()
                    if line.strip()
                )
                wt_dirty.update(
                    line.strip()
                    for line in _git_out(
                        workdir, ["ls-files", "--others", "--exclude-standard"]
                    ).splitlines()
                    if line.strip()
                )
                env_files = [
                    p
                    for p in env_files
                    if p in wt_dirty
                    or any(p == td or p.startswith(td + "/") for td in tdirs)
                ]
        return env_files

    if not workdir or not os.path.isdir(workdir):
        return []

    seen = set()
    result = []

    for cmd in (
        ["git", "-C", workdir, "diff", "--name-only", eff_base],
        ["git", "-C", workdir, "diff", "--cached", "--name-only"],
        ["git", "-C", workdir, "ls-files", "--others", "--exclude-standard"],
    ):
        try:
            out = subprocess.check_output(
                cmd, stderr=subprocess.DEVNULL, text=True
            )
        except Exception:
            continue
        for line in out.splitlines():
            rel = line.strip()
            if not rel or rel in seen:
                continue
            bname = os.path.basename(rel)
            if bname.startswith("CoderReport") and bname.endswith(".json"):
                continue
            seen.add(rel)
            result.append(rel)

    return result


import builtins as _builtins
import io as _io
import tempfile as _tempfile

_BUILD_FILENAME_RE = re.compile(
    r"(?:^|/)(?:BUILD|BUILD\.gn|BUILD\.bazel|MODULE\.bazel|[^/]+\.(?:bzl|gni))$"
)
_REAL_OPEN = _builtins.open
_truncated_blob_map = {}


# Check whether a working-tree build file is 0-byte truncated or unlinked on disk.
def _is_corrupted_worktree_build_candidate(full):
    try:
        if not os.path.lexists(full):
            return False, True
        if (
            not os.path.islink(full)
            and os.path.isfile(full)
            and os.path.getsize(full) == 0
        ):
            return True, False
    except OSError:
        pass
    return False, False


# Exclude intentional Case 1 BUILD.gn removals where sibling or overlaid BUILD.bazel is non-empty.
def _is_intentional_unstaged_gn_removal(wdir, full, rel, head_blob):
    if os.path.basename(rel) != "BUILD.gn" or os.path.lexists(full):
        return False
    bz_cands = [os.path.join(os.path.dirname(full), "BUILD.bazel")]
    pkg = os.path.dirname(rel).strip("/")
    if pkg.startswith("build/secondary/"):
        overlaid_pkg = pkg[len("build/secondary/") :].strip("/")
        if overlaid_pkg:
            bz_cands.append(os.path.join(wdir, overlaid_pkg, "BUILD.bazel"))
    sib_ok = False
    for bz_sib in bz_cands:
        try:
            if (
                not os.path.islink(bz_sib)
                and os.path.isfile(bz_sib)
                and os.path.getsize(bz_sib) > 0
            ):
                sib_ok = True
                break
        except OSError:
            pass
    return sib_ok and b"bazel_test_suite(" not in head_blob


# Collect tracked build files in `workdir` that were committed non-empty in a
# local migration `HEAD` commit but truncated to 0 bytes or unlinked on disk
# (for example after an interrupted write or revert on a degraded FUSE mount).
def find_truncated_head_build_blobs(workdir):
    """Maps paths of 0-byte or unlinked working-tree build files to `HEAD` bytes."""
    if not workdir or not os.path.isdir(workdir):
        return {}
    wdir = os.path.abspath(workdir)
    wait_for_tests_json_parity_restore(wdir)
    wt_diff = _git_out(wdir, ["diff", "--name-only", "HEAD"])
    if not wt_diff:
        return {}
    candidates = []
    for raw_ln in wt_diff.splitlines():
        rel = raw_ln.strip()
        if not rel or not _BUILD_FILENAME_RE.search(rel):
            continue
        full = os.path.join(wdir, rel)
        is_zero, is_missing = _is_corrupted_worktree_build_candidate(full)
        if not is_zero and not is_missing:
            continue
        candidates.append((rel, full, is_missing))
    if not candidates:
        return {}

    head_sha = _git_out(
        wdir, ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]
    )
    parent_sha = _git_out(
        wdir, ["rev-parse", "--verify", "--quiet", "HEAD~1^{commit}"]
    )
    if not head_sha or not parent_sha:
        return {}
    origin_main = _git_out(
        wdir, ["rev-parse", "--verify", "--quiet", "origin/main^{commit}"]
    )
    if origin_main and (
        head_sha == origin_main
        or _git_out(wdir, ["merge-base", origin_main, head_sha]) == head_sha
    ):
        return {}
    subj = _git_out(wdir, ["log", "-1", "--format=%s", "HEAD"])
    head_msg = _git_out(wdir, ["log", "-1", "--format=%B", "HEAD"])
    if re.search(r"(?m)^Reviewed-on:\s*\S", head_msg):
        return {}
    tdirs = with_overlaid_target_dirs(_resolve_active_target_dirs(wdir, None))
    head_files = [
        line.strip()
        for line in _git_out(
            wdir,
            [
                "diff",
                "--ignore-submodules=none",
                "--name-only",
                "HEAD~1",
                "HEAD",
            ],
        ).splitlines()
        if line.strip()
    ]
    if _is_prerequisite_stack_commit(
        wdir, head_msg, subj, head_files, tdirs, check_untracked=True
    ) or is_stacked_prerequisite_head(subj, head_msg, head_files, tdirs):
        return {}
    has_task_footer = bool(
        re.search(
            r"^(?:TAG\s*[:=]\s*.*\bplanter\b|Bazel-Migration-Target\s*:)",
            head_msg,
            re.M | re.I,
        )
    )
    is_mig_subj = (
        subj.startswith(("[bazel_migration]", "[bazel2gn]", "[bazel]"))
        or has_task_footer
    )
    head_touches_tdirs = any(
        _is_direct_target_pkg_file(wdir, f, td, head_files)
        for td in tdirs
        for f in head_files
    )
    if not (is_mig_subj or (head_touches_tdirs and bool(origin_main))):
        return {}
    if tdirs and not head_touches_tdirs:
        return {}

    staged = set(
        _git_out(wdir, ["diff", "--cached", "--name-only"]).splitlines()
    )
    recovered = {}
    for rel, full, is_missing in candidates:
        if rel in staged:
            continue
        try:
            blob = subprocess.check_output(
                ["git", "-C", wdir, "cat-file", "blob", f"HEAD:{rel}"],
                stderr=subprocess.DEVNULL,
            )
        except Exception:
            continue
        if not blob:
            continue
        if is_missing and _is_intentional_unstaged_gn_removal(
            wdir, full, rel, blob
        ):
            continue
        recovered[os.path.abspath(full)] = blob
        recovered[os.path.realpath(full)] = blob
    return recovered


# Return True if `path` has a recovered non-empty `HEAD` build blob in memory.
def has_recovered_build_blob(path):
    if not path:
        return False
    return os.path.abspath(path) in _truncated_blob_map


# Probe whether `.git` and candidate workspace directories accept non-empty file
# writes during full check runs, returning `(probe_dir, error)` on failure.
def probe_workspace_write_error(workdir, extra_dirs=()):
    skip_b = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
    dry_r = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
    if skip_b or dry_r or not workdir or not os.path.isdir(workdir):
        return None
    wdir = os.path.abspath(workdir)
    probe_dirs = []
    gdir = _git_out(wdir, ["rev-parse", "--absolute-git-dir"])
    if gdir and os.path.isdir(gdir):
        probe_dirs.append(gdir)
    for raw_d in list(extra_dirs or ()) + [wdir]:
        if not raw_d:
            continue
        cur = os.path.abspath(raw_d)
        while cur and not os.path.isdir(cur):
            nxt = os.path.dirname(cur)
            if nxt == cur:
                cur = ""
                break
            cur = nxt
        if cur and os.path.isdir(cur) and cur not in probe_dirs:
            probe_dirs.append(cur)
    probe_bytes = b"planter-write-probe\n"
    for pdir in probe_dirs:
        fd = None
        probe_path = None
        try:
            fd, probe_path = _tempfile.mkstemp(
                dir=pdir, prefix=".planter-write-probe."
            )
            os.write(fd, probe_bytes)
            os.close(fd)
            fd = None
            with _REAL_OPEN(probe_path, "rb") as rf:
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


# Install a read-only open() fallback so static checks evaluate committed `HEAD`
# bytes instead of emitting false-positive empty-file errors when a degraded
# mount leaves 0-byte truncated build files in the working tree.
def install_truncated_worktree_fallback(workdir=None):
    """Installs an in-memory `HEAD` reader for 0-byte truncated build files."""
    wdir = workdir or os.environ.get("PLANTER_WORKDIR", "").strip()
    if not wdir:
        return {}
    blobs = find_truncated_head_build_blobs(wdir)
    if not blobs:
        return {}
    _truncated_blob_map.update(blobs)

    def _fallback_open(
        file,
        mode="r",
        buffering=-1,
        encoding=None,
        errors=None,
        newline=None,
        closefd=True,
        opener=None,
    ):
        if (
            isinstance(file, (str, bytes, os.PathLike))
            and "r" in mode
            and not any(ch in mode for ch in ("w", "a", "x", "+"))
        ):
            try:
                raw_p = os.fsdecode(file)
                key = os.path.abspath(raw_p)
            except Exception:
                raw_p = None
                key = None
            data = _truncated_blob_map.get(key) if key else None
            if data is not None and any(
                _is_corrupted_worktree_build_candidate(key)
            ):
                raw_buf = _io.BytesIO(data)
                if raw_p:
                    try:
                        raw_buf.name = raw_p
                    except Exception:
                        pass
                if "b" in mode:
                    return raw_buf
                return _io.TextIOWrapper(
                    raw_buf,
                    encoding=encoding or "utf-8",
                    errors=errors or "strict",
                    newline=newline,
                )
        return _REAL_OPEN(
            file,
            mode=mode,
            buffering=buffering,
            encoding=encoding,
            errors=errors,
            newline=newline,
            closefd=closefd,
            opener=opener,
        )

    if _builtins.open is _REAL_OPEN:
        _builtins.open = _fallback_open
        _io.open = _fallback_open
    return blobs


if os.environ.get("PLANTER_WORKDIR", "").strip() and (
    os.environ.get("PLANTER_TARGET_DIR", "").strip()
    or os.environ.get("PLANTER_TARGET_DIRS", "").strip()
    or os.environ.get("PLANTER_CHECK_NAME", "").strip()
):
    install_truncated_worktree_fallback()


_review_diff_cache = {}


def read_review_diff_findings(workdir=None):
    """Loads precomputed review_diff findings from PLANTER_REVIEW_DIFF_JSON, or queries planter diff --local when unset."""
    if "PLANTER_REVIEW_DIFF_JSON" in os.environ:
        path = os.environ.get("PLANTER_REVIEW_DIFF_JSON", "").strip()
        if not path or not os.path.isfile(path):
            return []
        try:
            with open(path, "r", encoding="utf-8") as f:
                data = json.load(f)
            return data if isinstance(data, list) else []
        except Exception:
            return []
    if os.environ.get("PLANTER_SKIP_DIFF_COVERAGE") == "1":
        return []
    wdir = (
        workdir or os.environ.get("PLANTER_WORKDIR", "").strip() or os.getcwd()
    )
    if not wdir or not os.path.isdir(wdir):
        return []
    wdir = os.path.abspath(wdir)
    if wdir in _review_diff_cache:
        return list(_review_diff_cache[wdir])
    try:
        import rdeps_lib as rl

        planter_bin = rl.find_planter_bin()
    except Exception:
        planter_bin = None
    if not planter_bin:
        return []
    env = dict(
        os.environ,
        PLANTER_BIN=planter_bin,
        PLANTER_SKIP_DIFF_COVERAGE="1",
    )
    for flag in ("--format=findings-json", "--check-findings"):
        try:
            out = subprocess.run(
                [planter_bin, "diff", "--local", flag, f"--workdir={wdir}"],
                cwd=wdir,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=15,
            ).stdout.strip()
            findings, parsed_ok = parse_check_findings(out)
            if parsed_ok or out.startswith("["):
                _review_diff_cache[wdir] = findings
                return list(findings)
        except Exception:
            continue
    return []


def strip_comments(text, blank=False):
    """Strips `#` comments outside string literals while preserving line numbers.

    When `blank=True`, replaces comment characters with spaces so character offsets
    remain identical to `text`.
    """
    if not text:
        return ""
    out = []
    i = 0
    n = len(text)
    quote = None
    while i < n:
        c = text[i]
        if quote is not None:
            if c == "\\" and i + 1 < n:
                out.append(text[i : i + 2])
                i += 2
                continue
            if c == quote:
                quote = None
            out.append(c)
            i += 1
        elif c in ('"', "'"):
            quote = c
            out.append(c)
            i += 1
        elif c == "#":
            j = text.find("\n", i)
            if j < 0:
                j = n
            if blank:
                out.append(" " * (j - i))
            i = j
        else:
            out.append(c)
            i += 1
    return "".join(out)


def blank_comments(text):
    """Replaces `#` comments outside strings with spaces, preserving offsets and lines."""
    return strip_comments(text, blank=True)


# Allow //docs/ `{% includecode %}` repoints that follow a BUILD.gn region tag into
# BUILD.bazel. migration_sanity's includecode_region_tag_removed blocks removing a
# doc-referenced `# [START x]` / `# [END x]` region from BUILD.gn, so a migration that moves
# the region into the same directory's BUILD.bazel must repoint the directive in the same
# CL; build_only_scope and bazel_minimality accept exactly that docs edit and nothing else.
# Example: examples/components/routing echo packages (cl/1973114).
_INCLUDECODE_LINE_RE = re.compile(r"\{%\s*includecode\b.*%\}")
_INCLUDECODE_GERRIT_PATH_RE = re.compile(r'\bgerrit_path\s*=\s*"([^"]+)"')
_INCLUDECODE_REGION_TAG_RE = re.compile(r'\bregion_tag\s*=\s*"([^"]+)"')
_BUILD_REGION_MARKER_RE = re.compile(
    r"^\s*#\s*\[(START|END)\s+([^\]\s]+)\s*\]", re.M
)


def _complete_region_tags(text):
    """Returns region tags that have both `# [START t]` and `# [END t]` in `text`."""
    starts, ends = set(), set()
    for m in _BUILD_REGION_MARKER_RE.finditer(text or ""):
        (starts if m.group(1) == "START" else ends).add(m.group(2))
    return starts & ends


def _read_worktree_text(workdir, rel):
    try:
        with open(
            os.path.join(workdir, rel), encoding="utf-8", errors="replace"
        ) as f:
            return f.read()
    except OSError:
        return None


def _is_valid_includecode_repoint(workdir, change_base, old_line, new_line):
    """Checks one removed/added directive pair is a BUILD.gn -> BUILD.bazel region repoint."""
    old_gp = _INCLUDECODE_GERRIT_PATH_RE.search(old_line)
    new_gp = _INCLUDECODE_GERRIT_PATH_RE.search(new_line)
    tag_m = _INCLUDECODE_REGION_TAG_RE.search(old_line)
    if not (old_gp and new_gp and tag_m):
        return False
    gn_rel = old_gp.group(1).strip("/")
    if os.path.basename(gn_rel) != "BUILD.gn":
        return False
    bazel_rel = os.path.join(os.path.dirname(gn_rel), "BUILD.bazel")
    # The only allowed textual change is the gerrit_path value itself.
    expected = old_line.replace(
        old_gp.group(0), f'gerrit_path="{bazel_rel}"', 1
    )
    if new_gp.group(1).strip("/") != bazel_rel or new_line != expected:
        return False
    tag = tag_m.group(1).strip()
    # The BUILD.gn region must be gone: removed in this CL, or already dangling at the
    # change base because an earlier migration moved it into BUILD.bazel.
    cur_gn = _read_worktree_text(workdir, gn_rel)
    if cur_gn is not None and tag in _complete_region_tags(cur_gn):
        return False
    return tag in _complete_region_tags(_read_worktree_text(workdir, bazel_rel))


def is_includecode_repoint_edit(workdir, change_base, path):
    """True if every changed line of an existing docs/**/*.md file is an allowed repoint."""
    norm = path.strip("/")
    if not (norm.startswith("docs/") and norm.endswith(".md")):
        return False
    if (
        subprocess.call(
            ["git", "-C", workdir, "cat-file", "-e", f"{change_base}:{norm}"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        != 0
    ):
        return False  # Newly added docs files are never allowed.
    diff = _git_out(
        workdir, ["diff", "-U0", "--no-renames", change_base, "--", norm]
    )
    removed, added = [], []
    for line in diff.splitlines():
        if line.startswith(("+++", "---")):
            continue
        if line.startswith("-"):
            removed.append(line[1:])
        elif line.startswith("+"):
            added.append(line[1:])
    if not removed or len(removed) != len(added):
        return False
    if not all(_INCLUDECODE_LINE_RE.search(l) for l in removed + added):
        return False
    remaining = list(added)
    for old_line in removed:
        match = next(
            (
                n
                for n in remaining
                if _is_valid_includecode_repoint(
                    workdir, change_base, old_line, n
                )
            ),
            None,
        )
        if match is None:
            return False
        remaining.remove(match)
    return True


def has_sentinel(gn_text):
    """Returns True if `gn_text` contains a BAZEL2GN or LOCAL_BAZEL_BUILD sentinel line."""
    return bool(gn_text and SENTINEL_RE.search(gn_text))


def split_sentinel(gn_text):
    """Splits `gn_text` at the canonical BAZEL2GN SENTINEL line into `(above, below)`.

    If no sentinel line is present, returns `(gn_text, "")`.
    """
    if not gn_text:
        return "", ""
    m = SENTINEL_RE.search(gn_text)
    if not m:
        return gn_text, ""
    return gn_text[: m.start()], gn_text[m.end() :]


# Fuchsia-incompatible external Bazel repositories. migration_sanity, cq_reachability and
# build_verification share these helpers so they give one verdict and one remediation: a
# migrated test that reaches such a repository stays dual-built (synced below
# `## BAZEL2GN SENTINEL`, so its generated GN copy runs in CQ) and is not exported to any
# Bazel `test_suite` / GN `bazel_test_suite`.
_third_party_target_map_cache = {}
_fuchsia_incompat_ext_repos_cache = {}


def load_third_party_target_map(workdir):
    """Returns `build/tools/bazel2gn/third_party_target_map.json` (Bazel label -> GN label)."""
    wdir = os.path.abspath(workdir)
    if wdir not in _third_party_target_map_cache:
        tp_path = os.path.join(
            wdir, "build", "tools", "bazel2gn", "third_party_target_map.json"
        )
        try:
            with open(tp_path, encoding="utf-8") as tf:
                loaded = json.load(tf)
        except (OSError, ValueError):
            loaded = {}
        _third_party_target_map_cache[wdir] = (
            loaded if isinstance(loaded, dict) else {}
        )
    return _third_party_target_map_cache[wdir]


def fuchsia_incompatible_ext_repos(workdir):
    """Maps third-party Bzlmod modules (and their repo_name aliases) that cannot build for Fuchsia
    to the root-cause module.

    A module in `build/bazel/toplevel.MODULE.bazel` with a `local_path_override` under
    `third_party/` is incompatible when its upstream BUILD.bazel compiles `__Fuchsia__` sources
    that include `<fuchsia/...>` headers without Fuchsia SDK/FIDL deps, or when it depends on such a
    module.
    """
    wdir = os.path.abspath(workdir)
    if wdir in _fuchsia_incompat_ext_repos_cache:
        return _fuchsia_incompat_ext_repos_cache[wdir]
    incompat = {}
    mod_path = os.path.join(wdir, "build/bazel/toplevel.MODULE.bazel")
    try:
        with open(mod_path, encoding="utf-8") as mf:
            mod_txt = mf.read()
    except OSError:
        mod_txt = None
    if mod_txt is not None:
        repo_aliases = {}
        for m in re.finditer(
            r'bazel_dep\([^)]*\bname\s*=\s*"([^"]+)"[^)]*\brepo_name\s*=\s*"([^"]+)"',
            mod_txt,
            re.S,
        ):
            repo_aliases[m.group(1)] = m.group(2)
        mod_dirs = {}
        for m in re.finditer(
            r'local_path_override\(\s*module_name\s*=\s*"([^"]+)"\s*,\s*path\s*=\s*"([^"]+)"',
            mod_txt,
            re.S,
        ):
            mname, mpath = m.group(1), m.group(2)
            if mpath.startswith("third_party/"):
                mod_dirs[mname] = os.path.join(wdir, mpath)
        mod_deps = {m: set() for m in mod_dirs}
        for mname, mroot in mod_dirs.items():
            mfile = os.path.join(mroot, "MODULE.bazel")
            try:
                with open(mfile, encoding="utf-8") as mf:
                    mtxt = mf.read()
                for dm in re.finditer(
                    r'bazel_dep\(\s*name\s*=\s*"([^"]+)"', mtxt
                ):
                    mod_deps[mname].add(dm.group(1))
            except OSError:
                pass
            for cur_root, _, files in os.walk(mroot):
                if "BUILD.bazel" not in files:
                    continue
                try:
                    with open(
                        os.path.join(cur_root, "BUILD.bazel"), encoding="utf-8"
                    ) as bf:
                        btxt = bf.read()
                except OSError:
                    continue
                if "@fuchsia_sdk" in btxt or "//sdk/fidl/" in btxt:
                    continue
                for srel in re.findall(r'"([^"]+\.(?:cc|cpp|c|h))"', btxt):
                    sp = os.path.join(cur_root, srel)
                    if not os.path.isfile(sp):
                        continue
                    try:
                        with open(sp, encoding="utf-8", errors="ignore") as sf:
                            stxt = sf.read()
                    except OSError:
                        continue
                    if "__Fuchsia__" in stxt and re.search(
                        r"#\s*include\s*<fuchsia/[^>]+>", stxt
                    ):
                        incompat[mname] = mname
                        break
                if mname in incompat:
                    break
        changed_prop = True
        while changed_prop:
            changed_prop = False
            for mname, dset in mod_deps.items():
                if mname in incompat:
                    continue
                for dep_m in sorted(dset):
                    if dep_m in incompat:
                        incompat[mname] = incompat[dep_m]
                        changed_prop = True
                        break
        for mname, root_cause_m in list(incompat.items()):
            alias = repo_aliases.get(mname)
            if alias:
                incompat[alias] = root_cause_m
    _fuchsia_incompat_ext_repos_cache[wdir] = incompat
    return incompat


def _ext_repo_dep_labels(bazel_text, dep_labels):
    labels = set(re.findall(r'"(@[^"\s]+)"', bazel_text or ""))
    labels.update(l.strip() for l in (dep_labels or ()) if isinstance(l, str))
    return sorted(
        l
        for l in labels
        if l.startswith("@") and not l.startswith(("@//", "@@//"))
    )


def ext_repos_missing_angled_includes(
    workdir, pkg_dir, bazel_text, dep_labels=()
):
    """Returns `//third_party/<prefix>` repos whose upstream BUILD.bazel lacks `includes`.

    Covers external modules mapped in `third_party_target_map.json` that `bazel_text` /
    `dep_labels` reference, whose `third_party/<prefix>/src/BUILD.bazel` sets no `includes`,
    and whose angled `#include <prefix/...>` headers the package's C/C++ sources use (so a
    Fuchsia Bazel build of the package's test fails inside the upstream repo).
    """
    tp_map = load_third_party_target_map(workdir)
    if not tp_map:
        return []
    gn_labels = [
        gn_lbl
        for bz_lbl, gn_lbl in tp_map.items()
        if f'"{bz_lbl}"' in (bazel_text or "")
    ]
    for s in _ext_repo_dep_labels(bazel_text, dep_labels):
        repo_key = s.split("//", 1)[0].split(":", 1)[0]
        gn_labels.append(tp_map.get(s) or tp_map.get(repo_key) or "")
    prefixes = set()
    for gn_lbl in gn_labels:
        if not isinstance(gn_lbl, str) or not gn_lbl.startswith(
            "//third_party/"
        ):
            continue
        sub = gn_lbl[len("//third_party/") :].split(":", 1)[0].strip("/")
        if sub:
            prefixes.add(sub)
            prefixes.add(sub.split("/", 1)[0])
    needed_prefixes = []
    for pref in sorted(prefixes):
        up_bz = os.path.join(workdir, "third_party", pref, "src", "BUILD.bazel")
        try:
            with open(up_bz, encoding="utf-8") as uf:
                up_txt = uf.read()
        except OSError:
            continue
        if not re.search(r"\bincludes\s*=", up_txt):
            needed_prefixes.append(pref)
    if not needed_prefixes:
        return []
    matched = []
    pkg_abs = os.path.join(workdir, pkg_dir.strip("/"))
    try:
        for fn in sorted(os.listdir(pkg_abs)):
            if not fn.endswith((".cc", ".cpp", ".c", ".h", ".hpp")):
                continue
            with open(
                os.path.join(pkg_abs, fn), encoding="utf-8", errors="ignore"
            ) as sf:
                stxt = sf.read()
            for pref in needed_prefixes:
                lbl = f"//third_party/{pref}"
                if lbl not in matched and re.search(
                    r"#\s*include\s*<\s*" + re.escape(pref) + r"/", stxt
                ):
                    matched.append(lbl)
    except OSError:
        return matched
    return matched


def external_repo_test_blockers(
    workdir, pkg_dir, bazel_text, dep_labels=(), device=True
):
    """Returns the external repositories that keep a package's Bazel tests out of Bazel test suites.

    Combines `ext_repos_missing_angled_includes` (any test) with `@<repo>` deps on
    `fuchsia_incompatible_ext_repos` (device tests only, `device=True`).
    """
    blockers = list(
        ext_repos_missing_angled_includes(
            workdir, pkg_dir, bazel_text, dep_labels
        )
    )
    if device:
        incompat = fuchsia_incompatible_ext_repos(workdir)
        if incompat:
            for s in _ext_repo_dep_labels(bazel_text, dep_labels):
                repo_n = s.lstrip("@").split("//", 1)[0].split(":", 1)[0]
                if repo_n in incompat and f"@{repo_n}" not in blockers:
                    blockers.append(f"@{repo_n}")
    return sorted(blockers)


def external_repo_blocked_test_remediation(
    pkg_dir, test_desc="the Bazel test", blockers=()
):
    """Returns the shared remediation for a test blocked by a Fuchsia-incompatible external repo."""
    pkg = pkg_dir.strip("/")
    repo_txt = (
        ", ".join(f"`{b}`" for b in blockers)
        if blockers
        else "the external repository"
    )
    return (
        f"Keep {test_desc} and the other migrated targets in '{pkg}/BUILD.bazel', keep '{pkg}/BUILD.gn' "
        f"dual-built with them synced below `## BAZEL2GN SENTINEL` (re-run `fx bazel2gn -d {pkg}`) so "
        "the generated GN test copy runs in CQ, and do not list the Bazel test in any ancestor "
        "`BUILD.bazel` `test_suite` or GN `bazel_test_suite` (omit its ancestor-suite `visibility`) "
        f"while {repo_txt} cannot build for `@platforms//os:fuchsia`. Name the blocking repository "
        "in the summary and list only exit-0 commands in `Test:` footers and "
        "`CoderReport.test_footers`."
    )


def find_closing_delim(code, open_idx, open_ch="{", close_ch="}"):
    """Returns the index of `close_ch` matching `open_ch` at `code[open_idx]`, ignoring strings."""
    depth = 0
    quote = None
    i = open_idx
    n = len(code)
    while i < n:
        c = code[i]
        if quote is not None:
            if c == "\\" and i + 1 < n:
                i += 2
                continue
            if c == quote:
                quote = None
        elif c in ('"', "'"):
            quote = c
        elif c == open_ch:
            depth += 1
        elif c == close_ch:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return n


def quote_bare_gn_list_ident(code, name, value):
    """Replaces bare `name` identifiers used as GN list elements with `"value"`.

    GN `foreach(name, list) { deps += [ name ] }` bodies reference the loop
    variable without quotes; string-only interpolation would otherwise drop
    those list elements. String literals and member accesses are left intact.
    """
    if not name or name not in code:
        return code
    out = []
    i, n, depth = 0, len(code), 0
    while i < n:
        c = code[i]
        if c == '"':
            j = i + 1
            while j < n and code[j] != '"':
                j += 2 if code[j] == "\\" else 1
            out.append(code[i : j + 1])
            i = j + 1
            continue
        if c == "[":
            depth += 1
        elif c == "]":
            depth = max(0, depth - 1)
        elif depth > 0 and (c.isalpha() or c == "_"):
            m = re.match(r"[A-Za-z_]\w*", code[i:])
            tok = m.group(0)
            prev = code[i - 1] if i else ""
            end = i + len(tok)
            nxt = code[end] if end < n else ""
            if (
                tok == name
                and prev not in ".$"
                and not prev.isalnum()
                and nxt not in ".(["
            ):
                out.append('"' + value.replace('"', '\\"') + '"')
            else:
                out.append(tok)
            i = end
            continue
        out.append(c)
        i += 1
    return "".join(out)


def extract_gn_targets(gn_text, include_templates=False):
    """Extracts GN target blocks from `gn_text`.

    Returns a list of dicts with keys:
      `type`, `tmpl`, `rule`, `name`, `line`, `body`, `clean_body`, `start`, `end`.
    """
    if not gn_text:
        return []
    code = blank_comments(gn_text)
    targets = []
    for m in _GN_CALL_RE.finditer(code):
        tmpl = m.group(1)
        name = m.group(2)
        if tmpl in GN_NON_TARGET_KEYWORDS and not (
            include_templates and tmpl == "template"
        ):
            continue
        brace_idx = m.end() - 1
        close_idx = find_closing_delim(code, brace_idx, "{", "}")
        line_no = code.count("\n", 0, m.start(1)) + 1
        raw_body = gn_text[brace_idx + 1 : close_idx]
        clean_body = code[brace_idx + 1 : close_idx]
        targets.append(
            {
                "type": tmpl,
                "tmpl": tmpl,
                "rule": tmpl,
                "name": name,
                "line": line_no,
                "body": raw_body,
                "clean_body": clean_body,
                "start": m.start(1),
                "end": min(len(gn_text), close_idx + 1),
            }
        )
    return targets


def read_imported_gni_text(
    wdir, pkg_rel, raw_imp, base_rev="", prefer_base=False
):
    """Reads an imported `.gni` file from disk or `base_rev`, returning `(text, gni_dir)`."""
    imp = (raw_imp or "").strip()
    if not imp or not wdir:
        return "", ""
    clean_pkg = (pkg_rel or "").strip("/")
    if imp.startswith("//"):
        gni_rel = imp[2:].lstrip("/")
    else:
        gni_rel = os.path.normpath(os.path.join(clean_pkg, imp)).replace(
            "\\", "/"
        )
    if not gni_rel or gni_rel.startswith(".."):
        return "", ""
    gni_dir = os.path.dirname(gni_rel).strip("/")
    cands = [gni_rel]
    if not gni_rel.startswith("build/secondary/"):
        cands.append(f"build/secondary/{gni_rel}")

    def _from_base(rel_path):
        if not base_rev:
            return None
        try:
            return subprocess.check_output(
                ["git", "-C", wdir, "show", f"{base_rev}:{rel_path}"],
                stderr=subprocess.DEVNULL,
                text=True,
            )
        except Exception:
            return None

    def _from_disk(rel_path):
        abs_path = os.path.join(wdir, rel_path)
        if not os.path.isfile(abs_path):
            return None
        try:
            with open(abs_path, encoding="utf-8") as gf:
                return gf.read()
        except OSError:
            return None

    readers = (
        (_from_base, _from_disk) if prefer_base else (_from_disk, _from_base)
    )
    for cand_rel in cands:
        for reader in readers:
            txt = reader(cand_rel)
            if txt is not None:
                return txt, gni_dir
    return "", ""


def expand_gn_foreach_or_var_dep_items(
    gn_items, wdir, pkg_rel, base_rev="", tname="", base_gn_text=None
):
    """Expands target-scoped GN `foreach(...)` and `$var`/`${var}` (including `.gni` imports)."""
    if not gn_items or not any("$" in s for s in gn_items):
        return gn_items, False
    clean_pkg = (pkg_rel or "").strip("/")
    gn_rel = f"{clean_pkg}/BUILD.gn" if clean_pkg else "BUILD.gn"
    sec_rel = (
        f"build/secondary/{gn_rel}"
        if clean_pkg and not clean_pkg.startswith("build/secondary/")
        else ""
    )
    gn_cands = [gn_rel] + ([sec_rel] if sec_rel else [])
    base_gn = base_gn_text or ""
    if not base_gn and wdir and base_rev:
        for cand_rel in gn_cands:
            try:
                base_gn = subprocess.check_output(
                    ["git", "-C", wdir, "show", f"{base_rev}:{cand_rel}"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                )
                if base_gn:
                    break
            except Exception:
                base_gn = ""
    if not base_gn and wdir:
        for cand_rel in gn_cands:
            gn_path = os.path.join(wdir, cand_rel)
            if os.path.isfile(gn_path):
                try:
                    with open(gn_path, encoding="utf-8") as gf:
                        base_gn = gf.read()
                    if base_gn:
                        break
                except OSError:
                    base_gn = ""
    if not base_gn:
        return gn_items, False
    clean_gn = blank_comments(base_gn)
    str_lit_re = re.compile(r'"((?:[^"\\]|\\.)*)"')
    gn_target_re = re.compile(r'\b([A-Za-z_]\w*)\(\s*"([^"]+)"\s*\)\s*\{')
    skip_blk_re = re.compile(
        r"(?:template|declare_args|forward_variables_from)\s*\([^)]*\)\s*\{"
    )
    fe_blk_re = re.compile(
        r"\bforeach\s*\(\s*([A-Za-z_]\w*)\s*,\s*(?:([A-Za-z_]\w*)|(\[[^\]]*\]))\s*\)\s*\{"
    )
    not_targets = {
        "if",
        "else",
        "template",
        "declare_args",
        "foreach",
        "forward_variables_from",
    }
    target_str_attrs = {
        "name",
        "output_name",
        "crate_name",
        "source_root",
        "edition",
        "package_name",
        "component_name",
        "manifest",
        "test_type",
    }

    def _interp_gn_str(s, svars):
        if not svars or "$" not in s:
            return s
        out = s
        for _ in range(4):
            prev = out
            for k, v in svars.items():
                out = out.replace(f"${{{k}}}", v)
                out = re.sub(
                    rf"\${re.escape(k)}(?![A-Za-z0-9_])",
                    lambda _m, v=v: v,
                    out,
                )
            if out == prev or "$" not in out:
                break
        return out

    def _mask_gn_blocks(txt, mask_targets=True, mask_foreach=True):
        chars = list(txt)
        i, n, in_str = 0, len(txt), False
        while i < n:
            c = txt[i]
            if in_str:
                if c == "\\":
                    i += 2
                    continue
                in_str = c != '"'
                i += 1
                continue
            if c == '"':
                in_str = True
                i += 1
                continue
            if (c.isalpha() or c == "_") and (
                i == 0 or not (txt[i - 1].isalnum() or txt[i - 1] == "_")
            ):
                fm = fe_blk_re.match(txt, i) if mask_foreach else None
                sm = (
                    skip_blk_re.match(txt, i)
                    if not fm and mask_targets
                    else None
                )
                tm = (
                    gn_target_re.match(txt, i)
                    if not fm and not sm and mask_targets
                    else None
                )
                if fm or sm or (tm and tm.group(1) not in not_targets):
                    b_open = (fm or sm or tm).end() - 1
                    b_close = find_closing_delim(txt, b_open, "{", "}")
                    for k in range(b_open + 1, min(b_close, n)):
                        if chars[k] != "\n":
                            chars[k] = " "
                    i = b_close + 1
                    continue
            i += 1
        return "".join(chars)

    def _collect_scope(
        masked_txt, base_strs=None, base_lists=None, in_target=False
    ):
        s_vars = dict(base_strs or {})
        l_vars = {k: list(v) for k, v in (base_lists or {}).items()}
        events = []
        for sm in re.finditer(
            r'(?m)^\s*([A-Za-z_]\w*)\s*=\s*"((?:[^"\\]|\\.)*)"', masked_txt
        ):
            events.append((sm.start(), "str", sm))
        for vm in re.finditer(
            r"(?m)^\s*([A-Za-z_]\w*)\s*(\+?=)\s*\[", masked_txt
        ):
            events.append((vm.start(), "list", vm))
        events.sort(key=lambda e: e[0])
        for _, kind, m in events:
            vname = m.group(1)
            if kind == "str":
                if not (in_target and vname in target_str_attrs):
                    s_vars[vname] = _interp_gn_str(m.group(2), s_vars)
            else:
                b_open = m.end() - 1
                b_close = find_closing_delim(masked_txt, b_open, "[", "]")
                vals = [
                    _interp_gn_str(v, s_vars)
                    for v in str_lit_re.findall(
                        masked_txt[b_open + 1 : b_close]
                    )
                ]
                if m.group(2) == "+=":
                    l_vars.setdefault(vname, []).extend(vals)
                else:
                    l_vars[vname] = vals
        return s_vars, l_vars

    def _collect_foreach_headers(chunk, l_vars, s_vars):
        fe_map = {}
        for fm in fe_blk_re.finditer(chunk):
            vals = (
                l_vars.get(fm.group(2), [])
                if fm.group(2)
                else [
                    _interp_gn_str(v, s_vars)
                    for v in str_lit_re.findall(fm.group(3) or "")
                ]
            )
            if vals:
                fe_map[fm.group(1)] = vals
        return fe_map

    imp_strs, imp_lists, foreach_vars = {}, {}, {}
    if wdir:
        for raw_imp in re.findall(r'\bimport\(\s*"([^"$]+)"\s*\)', clean_gn):
            gni_txt, _ = read_imported_gni_text(
                wdir, clean_pkg, raw_imp, base_rev, prefer_base=bool(base_rev)
            )
            if gni_txt:
                clean_gni = blank_comments(gni_txt)
                imp_strs, imp_lists = _collect_scope(
                    _mask_gn_blocks(clean_gni), imp_strs, imp_lists
                )
                foreach_vars.update(
                    _collect_foreach_headers(
                        _mask_gn_blocks(
                            clean_gni, mask_targets=True, mask_foreach=False
                        ),
                        imp_lists,
                        imp_strs,
                    )
                )

    top_masked = _mask_gn_blocks(clean_gn)
    gn_str_vars, gn_list_vars = _collect_scope(top_masked, imp_strs, imp_lists)
    masked_no_fe = _mask_gn_blocks(
        clean_gn, mask_targets=True, mask_foreach=False
    )
    masked_fe_only = _mask_gn_blocks(
        clean_gn, mask_targets=False, mask_foreach=True
    )

    cand_names = [tname] if tname else [""]
    if tname:
        for suf in ("_unittest", "_test", "_bin"):
            if tname.endswith(suf) and len(tname) > len(suf):
                cand_names.append(tname[: -len(suf)])

    matched_target = False
    for want_tn in cand_names:
        for fm in fe_blk_re.finditer(masked_no_fe):
            if not want_tn:
                break
            lvar = fm.group(1)
            b_open = fm.end() - 1
            b_close = find_closing_delim(clean_gn, b_open, "{", "}")
            fbody = clean_gn[b_open + 1 : b_close]
            pre_strs, pre_lists = _collect_scope(
                top_masked[: fm.start()], imp_strs, imp_lists
            )
            domain_vals = (
                pre_lists.get(fm.group(2), gn_list_vars.get(fm.group(2), []))
                if fm.group(2)
                else [
                    _interp_gn_str(v, pre_strs)
                    for v in str_lit_re.findall(fm.group(3) or "")
                ]
            )
            if not domain_vals:
                continue
            masked_fbody = _mask_gn_blocks(fbody)
            for val in domain_vals:
                iter_strs = dict(pre_strs)
                iter_strs[lvar] = val
                iter_strs, iter_lists = _collect_scope(
                    masked_fbody, iter_strs, pre_lists
                )
                iter_strs[lvar] = val
                for tm in gn_target_re.finditer(fbody):
                    if tm.group(1) in not_targets:
                        continue
                    raw_tn = tm.group(2)
                    if _interp_gn_str(raw_tn, iter_strs) == want_tn:
                        matched_target = True
                        tb_open = tm.end() - 1
                        tb_close = find_closing_delim(fbody, tb_open, "{", "}")
                        tbody = fbody[tb_open + 1 : tb_close]
                        t_strs, t_lists = _collect_scope(
                            _mask_gn_blocks(tbody),
                            iter_strs,
                            iter_lists,
                            in_target=True,
                        )
                        if "$" in raw_tn:
                            gn_str_vars.update(t_strs)
                        else:
                            gn_str_vars.update(
                                {k: v for k, v in t_strs.items() if k != lvar}
                            )
                            foreach_vars[lvar] = domain_vals
                        gn_list_vars.update(t_lists)
                        foreach_vars.update(
                            _collect_foreach_headers(tbody, t_lists, t_strs)
                        )

        t_cursor = 0
        for tm in gn_target_re.finditer(masked_fe_only):
            if tm.start() < t_cursor or tm.group(1) in not_targets:
                continue
            tb_open = tm.end() - 1
            tb_close = find_closing_delim(clean_gn, tb_open, "{", "}")
            t_cursor = tb_close + 1
            pre_strs, pre_lists = _collect_scope(
                top_masked[: tm.start()], imp_strs, imp_lists
            )
            resolved_tname = _interp_gn_str(tm.group(2), pre_strs)
            if want_tn and resolved_tname != want_tn:
                continue
            matched_target = True
            tbody = clean_gn[tb_open + 1 : tb_close]
            t_strs, t_lists = _collect_scope(
                _mask_gn_blocks(tbody), pre_strs, pre_lists, in_target=True
            )
            gn_str_vars.update(t_strs)
            gn_list_vars.update(t_lists)
            foreach_vars.update(
                _collect_foreach_headers(tbody, t_lists, t_strs)
            )
        if matched_target:
            break

    if not matched_target:
        foreach_vars.update(
            _collect_foreach_headers(masked_no_fe, gn_list_vars, gn_str_vars)
        )

    var_token_re = re.compile(r"\$\{([A-Za-z_]\w*)\}|\$([A-Za-z_]\w*)")
    expanded = []
    did_expand = False
    for raw in gn_items:
        cur_list = [re.sub(r"\([^()]*\)$", "", raw).strip()]
        for _ in range(4):
            next_list = []
            changed = False
            for s in cur_list:
                vm = var_token_re.search(s)
                if not vm:
                    next_list.append(s)
                    continue
                vname = vm.group(1) or vm.group(2)
                tok_re = re.compile(
                    rf"\$\{{{re.escape(vname)}\}}|\${re.escape(vname)}(?![A-Za-z0-9_])"
                )
                if vname in gn_str_vars and vname not in foreach_vars:
                    val = gn_str_vars[vname]
                    next_list.append(tok_re.sub(lambda _m, v=val: v, s))
                    changed = True
                    did_expand = True
                else:
                    vals = foreach_vars.get(vname) or gn_list_vars.get(vname)
                    if vals:
                        for v in vals:
                            next_list.append(tok_re.sub(lambda _m, v=v: v, s))
                        changed = True
                        did_expand = True
                    elif vname in gn_str_vars:
                        val = gn_str_vars[vname]
                        next_list.append(tok_re.sub(lambda _m, v=val: v, s))
                        changed = True
                        did_expand = True
                    else:
                        return gn_items, False
            cur_list = next_list
            if not changed:
                break
        if any("$" in s for s in cur_list):
            return gn_items, False
        expanded.extend(cur_list)
    return expanded, did_expand


def has_bazel2gn_skip_comment(src_lines, call_lineno):
    """Returns True if the rule call at 1-based `call_lineno` is preceded by `# @bazel2gn:skip`."""
    idx = call_lineno - 2
    while idx >= 0:
        stripped_line = src_lines[idx].strip()
        if not stripped_line.startswith("#"):
            break
        if re.search(r"^#\s*@bazel2gn:skip\b", stripped_line):
            return True
        idx -= 1
    return False


def _extract_ast_strings(node, var_table=None, depth=0):
    if node is None or depth > 5:
        return []
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        return [node.value]
    if var_table and isinstance(node, ast.Name) and node.id in var_table:
        return _extract_ast_strings(var_table[node.id], var_table, depth + 1)
    if isinstance(node, (ast.List, ast.Tuple, ast.Set)):
        out = []
        for elt in node.elts:
            out.extend(_extract_ast_strings(elt, var_table, depth + 1))
        return out
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
        return _extract_ast_strings(
            node.left, var_table, depth + 1
        ) + _extract_ast_strings(node.right, var_table, depth + 1)
    if isinstance(node, ast.Call):
        out = []
        for arg in node.args:
            if isinstance(arg, ast.Dict):
                for v in arg.values:
                    out.extend(_extract_ast_strings(v, var_table, depth + 1))
            else:
                out.extend(_extract_ast_strings(arg, var_table, depth + 1))
        for kw in node.keywords:
            out.extend(_extract_ast_strings(kw.value, var_table, depth + 1))
        return out
    return []


def extract_bazel_targets(bazel_text, filename="<BUILD.bazel>"):
    """Extracts Bazel rule targets from `bazel_text`.

    Returns a list of dicts with keys:
      `rule`, `type`, `name`, `line`, `end_line`, `testonly`, `actual`,
      `visibility`, `deps`, `srcs`, `attrs`, `keywords`, `has_bazel2gn_skip`.
    """
    if not bazel_text:
        return []
    src_lines = bazel_text.splitlines()
    try:
        tree = ast.parse(bazel_text, filename=filename)
    except SyntaxError:
        return []

    var_table = {}
    for stmt in tree.body:
        if isinstance(stmt, ast.Assign):
            for tgt in stmt.targets:
                if isinstance(tgt, ast.Name):
                    var_table[tgt.id] = stmt.value

    targets = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        if isinstance(node.func, ast.Name):
            rule = node.func.id
        elif isinstance(node.func, ast.Attribute):
            rule = node.func.attr
        else:
            continue

        tname = None
        actual = None
        is_testonly = False
        kw_map = {}
        for kw in node.keywords:
            if not kw.arg:
                continue
            kw_map[kw.arg] = kw.value
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
            elif kw.arg == "testonly":
                if (
                    isinstance(kw.value, ast.Constant)
                    and kw.value.value is True
                ):
                    is_testonly = True
                elif isinstance(kw.value, ast.Name) and kw.value.id in (
                    "True",
                    "true",
                ):
                    is_testonly = True

        if not tname:
            continue

        vis = _extract_ast_strings(kw_map.get("visibility"), var_table)
        deps = _extract_ast_strings(kw_map.get("deps"), var_table)
        srcs = _extract_ast_strings(kw_map.get("srcs"), var_table)
        lineno = getattr(node, "lineno", 1)
        end_lineno = getattr(node, "end_lineno", lineno)
        targets.append(
            {
                "rule": rule,
                "type": rule,
                "name": tname,
                "line": lineno,
                "end_line": end_lineno,
                "testonly": is_testonly,
                "actual": actual,
                "visibility": vis,
                "deps": deps,
                "srcs": srcs,
                "attrs": set(kw_map.keys()),
                "keywords": kw_map,
                "has_bazel2gn_skip": has_bazel2gn_skip_comment(
                    src_lines, lineno
                ),
            }
        )

    targets.sort(key=lambda t: (t["line"], t["name"]))
    return targets


# Mirrors planter's substrate.Finding JSON fields (waiver: see visibility_audit.sh).
FINDING_FIELDS = (
    "source",
    "category",
    "severity",
    "file",
    "line",
    "message",
    "remediation",
    "waiver",
)


def new_finding(**kw):
    """Creates a finding dict with default empty fields."""
    f = {k: "" for k in FINDING_FIELDS}
    f["line"] = 0
    f.update(kw)
    return f


def is_blocking_finding(finding):
    """Returns True if `finding` has ERROR or WARNING severity."""
    return normalize_severity(finding.get("severity", "")) in (
        "ERROR",
        "WARNING",
    )


def _reject_constant(name):
    raise ValueError(f"invalid JSON constant {name}")


def _go_field(obj, name):
    value = None
    for k, v in obj.items():
        if (k == name or k.lower() == name) and v is not None:
            value = v
    return value


def load_check_manifest(path):
    """Parses `checks/manifest.json` using Go-compatible case-insensitive field matching."""
    try:
        with open(path, encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return None
    try:
        doc = json.loads(text, parse_constant=_reject_constant)
    except ValueError as e:
        raise ValueError(f"invalid JSON: {e}") from e
    if doc is None:
        return []
    if not isinstance(doc, dict):
        raise ValueError("the manifest is not a JSON object")
    version = _go_field(doc, "version")
    if version is not None and (
        isinstance(version, bool) or not isinstance(version, int)
    ):
        raise ValueError('"version" is not an integer')
    checks = _go_field(doc, "checks")
    if checks is None:
        return []
    if not isinstance(checks, list):
        raise ValueError('"checks" is not a list')
    specs = []
    for n, c in enumerate(checks):
        c = {} if c is None else c
        if not isinstance(c, dict):
            raise ValueError(f"checks[{n}] is not an object")
        spec = {
            "name": "",
            "description": "",
            "script_file": "",
            "args": [],
            "enabled": False,
        }
        for field in ("name", "description", "script_file"):
            v = _go_field(c, field)
            if v is not None:
                if not isinstance(v, str):
                    raise ValueError(f"checks[{n}].{field} is not a string")
                spec[field] = v
        v = _go_field(c, "args")
        if v is not None:
            if not isinstance(v, list) or not all(
                a is None or isinstance(a, str) for a in v
            ):
                raise ValueError(f"checks[{n}].args is not a list of strings")
            spec["args"] = ["" if a is None else a for a in v]
        v = _go_field(c, "enabled")
        if v is not None:
            if not isinstance(v, bool):
                raise ValueError(f"checks[{n}].enabled is not a boolean")
            spec["enabled"] = v
        specs.append(spec)
    return specs


def _decode_findings(value):
    ok = True
    out = []
    for el in value:
        f = new_finding()
        if isinstance(el, dict):
            for k, v in el.items():
                name = k if k in f else k.lower() if k.lower() in f else None
                if name is None or v is None:
                    continue
                if name == "line":
                    if (
                        isinstance(v, int)
                        and not isinstance(v, bool)
                        and -(1 << 63) <= v < (1 << 63)
                    ):
                        f["line"] = v
                    else:
                        ok = False
                elif isinstance(v, str):
                    f[name] = v
                else:
                    ok = False
        elif el is not None:
            ok = False
        out.append(f)
    return out, ok


def parse_check_findings(raw):
    """Parses JSON findings output from a check script, recovering trailing JSON arrays if needed."""
    if not raw:
        return [], False
    partial = []
    try:
        whole = json.loads(raw, parse_constant=_reject_constant)
    except ValueError:
        whole = ValueError
    if whole is None:
        return [], True
    if isinstance(whole, list):
        findings, ok = _decode_findings(whole)
        if ok:
            return findings, True
        partial = findings
    decoder = json.JSONDecoder(parse_constant=_reject_constant)
    candidates = []
    start = raw.find("[")
    while start != -1:
        try:
            value, end = decoder.raw_decode(raw, start)
            candidates.append((end, start, value))
        except ValueError:
            pass
        start = raw.find("[", start + 1)
    candidates.sort(key=lambda c: (-c[0], c[1]))
    for _, _, value in candidates:
        findings, ok = _decode_findings(value)
        if ok:
            return findings, True
        partial = findings
    return partial, False


SIGNAL_NAMES = {
    signal.SIGHUP: "hangup",
    signal.SIGINT: "interrupt",
    signal.SIGQUIT: "quit",
    signal.SIGABRT: "aborted",
    signal.SIGBUS: "bus error",
    signal.SIGFPE: "floating point exception",
    signal.SIGKILL: "killed",
    signal.SIGSEGV: "segmentation fault",
    signal.SIGPIPE: "broken pipe",
    signal.SIGTERM: "terminated",
}


def fmt_secs(secs):
    """Formats seconds as e.g. '1.2s' or '2m05s'."""
    return (
        f"{secs:.1f}s"
        if secs < 60
        else f"{int(secs // 60)}m{int(secs % 60):02d}s"
    )


def _stop_proc_tree(proc):
    if proc.poll() is not None:
        return
    children = {}
    for entry in os.listdir("/proc") if os.path.isdir("/proc") else []:
        if entry.isdigit():
            try:
                with open(f"/proc/{entry}/stat") as f:
                    stat = f.read()
                ppid = int(stat[stat.rindex(")") + 2 :].split()[1])
                children.setdefault(ppid, []).append(int(entry))
            except (OSError, ValueError, IndexError):
                pass
    pids, todo = [proc.pid], [proc.pid]
    while todo:
        for child in children.get(todo.pop(), []):
            pids.append(child)
            todo.append(child)
    for sig, wait_s in ((signal.SIGTERM, 10), (signal.SIGKILL, None)):
        for pid in pids:
            try:
                os.kill(pid, sig)
            except OSError:
                pass
        if wait_s:
            try:
                proc.wait(timeout=wait_s)
                return
            except subprocess.TimeoutExpired:
                pass
    proc.wait()


def execute_subprocess(cmd, cwd, env, label="", heartbeat_secs=60, log_fn=None):
    """Runs `cmd` with periodic heartbeat logs and full process-tree cleanup on interrupt."""
    try:
        proc = subprocess.Popen(
            cmd,
            cwd=cwd,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as e:
        return "", "", str(e)
    started = time.monotonic()
    try:
        while True:
            try:
                out, err = proc.communicate(timeout=heartbeat_secs)
                break
            except subprocess.TimeoutExpired:
                if log_fn:
                    log_fn(
                        f"{label} still running ({fmt_secs(time.monotonic() - started)})"
                    )
    except BaseException:
        _stop_proc_tree(proc)
        raise
    rc = proc.returncode
    err_text = (
        None
        if rc == 0
        else (
            f"exit status {rc}"
            if rc > 0
            else f"signal: {SIGNAL_NAMES.get(-rc, str(-rc))}"
        )
    )
    return (
        out.decode("utf-8", "replace"),
        err.decode("utf-8", "replace"),
        err_text,
    )


def extract_gn_decomposed_attr_items(base_gn, tname, gn_attr):
    """Extracts string literals assigned to `gn_attr` on `tname` in `base_gn`.

    Expands same-file GN `template(...)` wrappers invoked by `tname` and file or
    target/template GN list variables. Returns `(resolved_ok, items)`.
    """
    clean_base = blank_comments(base_gn)
    tmpl_bodies = {}
    masked_chars = list(clean_base)
    for tm in re.finditer(r'\btemplate\(\s*"([^"$]+)"\s*\)\s*\{', clean_base):
        open_idx = tm.end() - 1
        close_idx = find_closing_delim(clean_base, open_idx, "{", "}")
        if close_idx and close_idx > open_idx:
            tmpl_bodies[tm.group(1)] = clean_base[open_idx + 1 : close_idx]
            for k in range(tm.start(), min(close_idx + 1, len(masked_chars))):
                if masked_chars[k] != "\n":
                    masked_chars[k] = " "
    for cm in re.finditer(
        r'\b([A-Za-z_]\w*)\(\s*"[^"$]*"\s*\)\s*\{', clean_base
    ):
        if cm.group(1) in GN_NON_TARGET_KEYWORDS:
            continue
        open_idx = cm.end() - 1
        close_idx = find_closing_delim(clean_base, open_idx, "{", "}")
        if close_idx and close_idx > open_idx:
            for k in range(cm.start(), min(close_idx + 1, len(masked_chars))):
                if masked_chars[k] != "\n":
                    masked_chars[k] = " "

    def _parse_gn_list_rhs(src, pos, known_vars, allow_invoker=False):
        vals = []
        n = len(src)
        saw_term = False
        while True:
            while pos < n and src[pos].isspace():
                pos += 1
            if pos >= n:
                return (vals, True) if saw_term else ([], False)
            if src[pos] == "[":
                end_b = find_closing_delim(src, pos, "[", "]")
                if end_b <= pos or end_b >= n or src[end_b] != "]":
                    return [], False
                inner = src[pos + 1 : end_b]
                if re.sub(r'"(?:[^"\\]|\\.)*"|[\s,]', "", inner):
                    return [], False
                vals.extend(re.findall(r'"((?:[^"\\]|\\.)*)"', inner))
                pos = end_b + 1
                saw_term = True
            else:
                idm = re.match(r"([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?)", src[pos:])
                if not idm:
                    return [], False
                ident = idm.group(1)
                pos += idm.end()
                if pos < n and src[pos] == "(":
                    return [], False
                if allow_invoker and ident == f"invoker.{gn_attr}":
                    saw_term = True
                elif known_vars.get(ident) is not None:
                    vals.extend(known_vars[ident])
                    saw_term = True
                else:
                    return [], False
            ws = pos
            while ws < n and src[ws].isspace():
                ws += 1
            if (
                ws < n
                and src[ws] == "+"
                and (ws + 1 >= n or src[ws + 1] != "=")
            ):
                pos = ws + 1
                continue
            break
        return vals, saw_term

    def _apply_gn_var_assign(var_map, vname, op, p_vals, ok, in_branch):
        if not ok:
            var_map[vname] = None
        elif op == "=":
            if not in_branch:
                var_map[vname] = list(p_vals)
            elif vname not in var_map:
                var_map[vname] = list(p_vals)
            elif var_map[vname] is not None:
                var_map[vname].extend(p_vals)
        elif op == "+=":
            if var_map.get(vname) is not None:
                var_map[vname].extend(p_vals)
            else:
                var_map[vname] = None

    def _blank_gn_strings(src):
        return re.sub(
            r'"(?:\\.|[^"\\])*"', lambda m: " " * len(m.group(0)), src
        )

    masked_top = "".join(masked_chars)
    masked_top_nostr = _blank_gn_strings(masked_top)
    gn_list_vars = {}
    for vm in re.finditer(r"(?m)^[ \t]*([A-Za-z_]\w*)\s*(\+?=)\s*", masked_top):
        vname, op = vm.group(1), vm.group(2)
        in_branch = masked_top_nostr.count(
            "{", 0, vm.start()
        ) > masked_top_nostr.count("}", 0, vm.start())
        p_vals, ok = _parse_gn_list_rhs(masked_top, vm.end(), gn_list_vars)
        _apply_gn_var_assign(gn_list_vars, vname, op, p_vals, ok, in_branch)

    if re.search(r"\bforward_variables_from\s*\(", masked_top_nostr):
        return False, []

    known_builtin_tmpls = {
        "rustc_library",
        "rustc_binary",
        "rustc_proc_macro",
        "rustc_test",
        "ffx_plugin",
        "ffx_tool",
        "idk_ffx_tool",
        "rustc_binary_host_tool",
        "idk_rustc_binary_host_tool",
        "sdk_rustc_binary_host_tool",
        "fuchsia_unittest_package",
        "fuchsia_unittest_component",
        "fuchsia_test_package",
        "fuchsia_test_component",
        "group",
        "host_test_data",
        "install_host_tools",
        "install_python_tool",
    }
    gn_ctrl_calls = {
        "if",
        "foreach",
        "template",
        "declare_args",
        "set_defaults",
        "set_default_toolchain",
        "toolchain",
        "config",
        "pool",
    }
    items = []
    found_owner = False
    for gt in extract_gn_targets(base_gn):
        if gt["name"] != tname:
            continue
        found_owner = True
        tmpl_nm = gt.get("tmpl", "")
        if not tmpl_nm:
            return False, []
        tmpl_chain = []
        queue = [tmpl_nm]
        seen_tmpls = set()
        while queue:
            cur_t = queue.pop(0)
            if cur_t in seen_tmpls:
                continue
            seen_tmpls.add(cur_t)
            if cur_t in tmpl_bodies:
                t_body = tmpl_bodies[cur_t]
                tmpl_chain.append(t_body)
                t_nostr = _blank_gn_strings(t_body)
                for cm in re.finditer(r"\b([A-Za-z_]\w*)\s*\(", t_nostr):
                    callee = cm.group(1)
                    p_idx = cm.end()
                    depth = 1
                    while p_idx < len(t_nostr) and depth > 0:
                        if t_nostr[p_idx] == "(":
                            depth += 1
                        elif t_nostr[p_idx] == ")":
                            depth -= 1
                        p_idx += 1
                    while p_idx < len(t_nostr) and t_nostr[p_idx].isspace():
                        p_idx += 1
                    if (
                        depth != 0
                        or p_idx >= len(t_nostr)
                        or t_nostr[p_idx] != "{"
                    ):
                        continue
                    if callee in gn_ctrl_calls:
                        continue
                    if callee in tmpl_bodies:
                        queue.append(callee)
                    elif callee not in known_builtin_tmpls:
                        return False, []
            elif cur_t not in known_builtin_tmpls:
                return False, []
        for is_tmpl, body in [(True, tb) for tb in tmpl_chain] + [
            (False, gt["clean_body"])
        ]:
            if not body:
                continue
            body_nostr = _blank_gn_strings(body)
            for fm in re.finditer(
                r"\bforward_variables_from\s*\(\s*(?:(invoker)\s*,)?",
                body_nostr,
            ):
                if not is_tmpl or fm.group(1) != "invoker":
                    return False, []
            local_vars = {
                k: (list(v) if v is not None else None)
                for k, v in gn_list_vars.items()
            }
            for vm in re.finditer(r"(?m)^\s*([A-Za-z_]\w*)\s*(\+?=)\s*", body):
                vname, op = vm.group(1), vm.group(2)
                if vname != gn_attr:
                    in_branch = body_nostr.count(
                        "{", 0, vm.start()
                    ) > body_nostr.count("}", 0, vm.start())
                    p_vals, ok = _parse_gn_list_rhs(
                        body, vm.end(), local_vars, allow_invoker=is_tmpl
                    )
                    _apply_gn_var_assign(
                        local_vars, vname, op, p_vals, ok, in_branch
                    )
                    continue
                p_vals, ok = _parse_gn_list_rhs(
                    body, vm.end(), local_vars, allow_invoker=is_tmpl
                )
                if not ok:
                    return False, []
                items.extend(p_vals)
    if not found_owner:
        return False, []
    return True, items


_RUST_CHAR_RE = re.compile(
    r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F_]+\}|.)|[^'\\\n])'"
)


def _skip_rust_string_or_char(text, i):
    """Returns end index after a Rust string/char literal starting at `i`, else `i`."""
    n = len(text)
    at_boundary = i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")
    if at_boundary:
        raw_m = re.match(r'(?:b|c)?r(#*)"', text[i:])
        if raw_m:
            close_seq = '"' + raw_m.group(1)
            j = text.find(close_seq, i + raw_m.end())
            return n if j < 0 else j + len(close_seq)
        if text.startswith("b'", i):
            ch_m = _RUST_CHAR_RE.match(text, i + 1)
            if ch_m:
                return ch_m.end()
    if text[i] == '"' or (at_boundary and text.startswith(('b"', 'c"'), i)):
        k = i + (2 if text[i] != '"' else 1)
        while k < n:
            if text[k] == "\\":
                k += 2
                continue
            if text[k] == '"':
                return k + 1
            k += 1
        return n
    if text[i] == "'":
        ch_m = _RUST_CHAR_RE.match(text, i)
        if ch_m:
            return ch_m.end()
    return i


def _blank_preserving_newlines(chunk):
    return "".join("\n" if ch == "\n" else " " for ch in chunk)


def strip_rust_comments(text, blank_strings=False):
    """Replaces Rust comments (and optionally string/char literals) with same-length spaces."""
    if not text:
        return ""
    out, i, n = [], 0, len(text)
    while i < n:
        if text.startswith("//", i):
            j = text.find("\n", i + 2)
            end = n if j < 0 else j
            out.append(" " * (end - i))
            i = end
            continue
        if text.startswith("/*", i):
            depth, k = 1, i + 2
            while k < n and depth > 0:
                if text.startswith("/*", k):
                    depth, k = depth + 1, k + 2
                elif text.startswith("*/", k):
                    depth, k = depth - 1, k + 2
                else:
                    k += 1
            out.append(_blank_preserving_newlines(text[i:k]))
            i = k
            continue
        end_lit = _skip_rust_string_or_char(text, i)
        if end_lit > i:
            out.append(
                _blank_preserving_newlines(text[i:end_lit])
                if blank_strings
                else text[i:end_lit]
            )
            i = end_lit
            continue
        out.append(text[i])
        i += 1
    return "".join(out)


def _is_rust_cfg_test_only_attr(attr_inner):
    s = attr_inner.strip()
    if (
        s == "test"
        or re.match(
            r"^(?:::\s*)?[A-Za-z_]\w*(?:\s*::\s*[A-Za-z_]\w*)*\s*::\s*test(?:\s*\(.*\))?$",
            s,
            re.S,
        )
        or re.match(r"^(?:[\w:]+\s*::\s*)?test_case\s*\(.*\)$", s, re.S)
    ):
        return True
    m = re.match(r"^cfg\s*\((.*)\)\s*$", s, re.S)
    if not m:
        return False
    cfg_body = m.group(1).strip()
    if cfg_body == "test":
        return True
    m_all = re.match(r"^all\s*\((.*)\)\s*$", cfg_body, re.S)
    if not m_all:
        return False
    inner = m_all.group(1)
    depth, start = 0, 0
    k, n = 0, len(inner)
    terms = []
    while k < n:
        end_lit = _skip_rust_string_or_char(inner, k)
        if end_lit > k:
            k = end_lit
            continue
        if inner[k] == "(":
            depth += 1
        elif inner[k] == ")" and depth > 0:
            depth -= 1
        elif inner[k] == "," and depth == 0:
            terms.append(inner[start:k].strip())
            start = k + 1
        k += 1
    terms.append(inner[start:].strip())
    return "test" in terms


def _find_rust_bracket_close(code, open_idx, open_ch, close_ch):
    n = len(code)
    depth = 1
    k = open_idx + 1
    while k < n and depth > 0:
        end_lit = _skip_rust_string_or_char(code, k)
        if end_lit > k:
            k = end_lit
            continue
        if code[k] == open_ch:
            depth += 1
        elif code[k] == close_ch:
            depth -= 1
            if depth == 0:
                return k
        k += 1
    return n - 1


def strip_rust_cfg_test(text):
    """Strips Rust comments and `#[cfg(test)]` / `#[test]` items, preserving string literals."""
    code = strip_rust_comments(text, blank_strings=False)
    if not code:
        return ""
    chars = list(code)
    i, n = 0, len(code)
    top_brace_depth = 0
    while i < n:
        end_lit = _skip_rust_string_or_char(code, i)
        if end_lit > i:
            i = end_lit
            continue
        if code[i] == "{":
            top_brace_depth += 1
            i += 1
            continue
        if code[i] == "}":
            if top_brace_depth > 0:
                top_brace_depth -= 1
            i += 1
            continue
        if code[i] == "#":
            k = i + 1
            while k < n and code[k].isspace():
                k += 1
            is_inner = False
            if k < n and code[k] == "!":
                is_inner = True
                k += 1
                while k < n and code[k].isspace():
                    k += 1
            if k < n and code[k] == "[":
                b_close = _find_rust_bracket_close(code, k, "[", "]")
                attr_inner = code[k + 1 : b_close]
                if _is_rust_cfg_test_only_attr(attr_inner):
                    if is_inner and top_brace_depth == 0:
                        return _blank_preserving_newlines(code)
                    if not is_inner:
                        j = b_close + 1
                        while j < n:
                            while j < n and code[j].isspace():
                                j += 1
                            if j < n and code[j] == "#":
                                a_pos = j + 1
                                while a_pos < n and code[a_pos].isspace():
                                    a_pos += 1
                                if a_pos < n and code[a_pos] == "[":
                                    j = (
                                        _find_rust_bracket_close(
                                            code, a_pos, "[", "]"
                                        )
                                        + 1
                                    )
                                    continue
                            break
                        rem = code[j:]
                        semi_only = False
                        m_kw = re.match(
                            r"(?:pub(?:\s*\([^)]*\))?\s+)*(?:default\s+)?([A-Za-z_]\w*)\b",
                            rem,
                        )
                        if m_kw:
                            kw = m_kw.group(1)
                            if kw in ("use", "type", "static", "let"):
                                semi_only = True
                            elif kw == "const" and not re.match(
                                r"(?:pub(?:\s*\([^)]*\))?\s+)*const\s+(?:unsafe\s+|async\s+|extern(?:\s+\"[^\"]*\")?\s+)*fn\b",
                                rem,
                            ):
                                semi_only = True
                        end_item = n
                        paren_d = bracket_d = brace_d = 0
                        p = j
                        while p < n:
                            p_lit = _skip_rust_string_or_char(code, p)
                            if p_lit > p:
                                p = p_lit
                                continue
                            ch = code[p]
                            if ch == "(":
                                paren_d += 1
                            elif ch == ")" and paren_d > 0:
                                paren_d -= 1
                            elif ch == "[":
                                bracket_d += 1
                            elif ch == "]" and bracket_d > 0:
                                bracket_d -= 1
                            elif ch == "{":
                                if (
                                    not semi_only
                                    and paren_d == 0
                                    and bracket_d == 0
                                ):
                                    end_item = (
                                        _find_rust_bracket_close(
                                            code, p, "{", "}"
                                        )
                                        + 1
                                    )
                                    break
                                brace_d += 1
                            elif ch == "}" and brace_d > 0:
                                brace_d -= 1
                            elif (
                                ch == ";"
                                and paren_d == 0
                                and bracket_d == 0
                                and brace_d == 0
                            ):
                                end_item = p + 1
                                break
                            p += 1
                        for idx in range(i, min(end_item, n)):
                            if chars[idx] != "\n":
                                chars[idx] = " "
                        i = end_item
                        continue
                i = b_close + 1
                continue
        i += 1
    return "".join(chars)


def extract_rust_submodules(text, include_cfg_test=True):
    """Returns `(mod_name, explicit_paths)` for external `mod <name>;` items in `text`."""
    code = (
        strip_rust_comments(text, blank_strings=False)
        if include_cfg_test
        else strip_rust_cfg_test(text)
    )
    if not code:
        return []
    code_nostr = strip_rust_comments(code, blank_strings=True)
    out = []
    for mm in re.finditer(
        r"(?:pub(?:\s*\([^)]*\))?\s+)?\bmod\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s*;",
        code_nostr,
    ):
        mod_name = mm.group(1)
        pos = mm.start()
        attr_start = pos
        while True:
            k = attr_start - 1
            while k >= 0 and code_nostr[k].isspace():
                k -= 1
            if k < 0 or code_nostr[k] != "]":
                break
            depth = 1
            k -= 1
            while k >= 0 and depth > 0:
                if code_nostr[k] == "]":
                    depth += 1
                elif code_nostr[k] == "[":
                    depth -= 1
                k -= 1
            if depth != 0:
                break
            while k >= 0 and code_nostr[k].isspace():
                k -= 1
            if k < 0 or code_nostr[k] != "#":
                break
            attr_start = k
        attr_blob = code[attr_start:pos]
        explicit_paths = []
        for pm in re.finditer(
            r'(?<![\w.])path\s*=\s*(?:r(#*)"(.*?)"\1|"((?:\\.|[^"\\])*)")',
            attr_blob,
            re.S,
        ):
            val = pm.group(2) if pm.group(2) is not None else pm.group(3)
            if val:
                explicit_paths.append(val)
        out.append((mod_name, explicit_paths))
    return out


def rust_target_non_test_sources_embed(
    workdir, pkg_rel, tname, bz_text, base_name
):
    """True when non-`#[cfg(test)]` Rust sources of `tname` embed `base_name` via `include*!`."""
    if not workdir or not base_name or not bz_text:
        return False
    clean_pkg = (pkg_rel or "").strip("/")
    var_table = {}
    try:
        tree = ast.parse(bz_text)
    except SyntaxError:
        return False
    for st in tree.body:
        if isinstance(st, ast.Assign):
            for tgt in st.targets:
                if isinstance(tgt, ast.Name):
                    var_table[tgt.id] = st.value
    crate_specs = []
    for bt in extract_bazel_targets(bz_text):
        if bt["name"] != tname:
            continue
        kws = bt.get("keywords", {})
        rule = bt.get("rule", "")
        exec_roots = [
            s
            for rk in ("crate_root", "main")
            for s in _extract_ast_strings(kws.get(rk), var_table)
            if s.endswith(".rs")
        ]
        exec_srcs = [
            s
            for s in _extract_ast_strings(kws.get("srcs"), var_table)
            if s.endswith(".rs")
        ]
        args_roots = [
            s
            for rk in ("args_crate_root", "args_source_root")
            for s in _extract_ast_strings(kws.get(rk), var_table)
            if s.endswith(".rs")
        ]
        args_srcs = [
            s
            for sk in ("args_srcs", "args_sources")
            for s in _extract_ast_strings(kws.get(sk), var_table)
            if s.endswith(".rs")
        ]
        if rule == "ffx_plugin":
            has_exec = bool(
                exec_roots
                or exec_srcs
                or (
                    "srcs" in kws
                    and not (
                        isinstance(kws["srcs"], ast.List)
                        and not kws["srcs"].elts
                    )
                )
            )
            if has_exec:
                crate_specs.append(
                    (
                        exec_roots,
                        exec_srcs,
                        ("lib.rs", "main.rs"),
                        ("src/lib.rs", "lib.rs"),
                    )
                )
            crate_specs.append(
                (
                    args_roots,
                    args_srcs,
                    ("args.rs",),
                    ("src/args.rs", "args.rs"),
                )
            )
        else:
            crate_specs.append(
                (
                    exec_roots,
                    exec_srcs,
                    ("lib.rs", "main.rs"),
                    ("src/lib.rs", "src/main.rs", "lib.rs", "main.rs"),
                )
            )
            if args_roots or args_srcs:
                crate_specs.append(
                    (
                        args_roots,
                        args_srcs,
                        ("args.rs",),
                        ("src/args.rs", "args.rs"),
                    )
                )

    def _to_repo_rel(raw):
        r = (raw or "").strip()
        if not r or "$" in r or r.startswith("@"):
            return ""
        if r.startswith("//"):
            p, sep, fpart = r[2:].partition(":")
            cand = (
                f"{p.strip('/')}/{fpart.lstrip('/')}" if sep else p.strip("/")
            )
        else:
            fpart = r.lstrip(":/")
            cand = f"{clean_pkg}/{fpart}".strip("/") if clean_pkg else fpart
        cand = os.path.normpath(cand).replace("\\", "/").strip("/")
        return "" if cand.startswith("..") else cand

    def _cands_for_mod(cur_rs, mod_name, explicit_paths, is_seed=False):
        bdir = os.path.dirname(cur_rs)
        st = os.path.splitext(os.path.basename(cur_rs))[0]
        sdirs = [bdir]
        if st not in ("lib", "main", "mod", "args") and not is_seed:
            sdirs.append(os.path.join(bdir, st))
        cands = []
        if explicit_paths:
            for ep in explicit_paths:
                for sd in dict.fromkeys([bdir, *sdirs]):
                    cands.append(
                        os.path.normpath(os.path.join(sd, ep)).replace(
                            "\\", "/"
                        )
                    )
        else:
            for sd in sdirs:
                cands.append(
                    os.path.normpath(
                        os.path.join(sd, f"{mod_name}.rs")
                    ).replace("\\", "/")
                )
                cands.append(
                    os.path.normpath(
                        os.path.join(sd, mod_name, "mod.rs")
                    ).replace("\\", "/")
                )
        return cands

    def _resolve_crate_seeds(c_roots, c_srcs, std_names, default_cands):
        c_seeds = []
        for raw in c_roots:
            rr = _to_repo_rel(raw)
            if rr and os.path.isfile(os.path.join(workdir, rr)):
                c_seeds.append(rr)
        if not c_seeds and c_srcs:
            norm_srcs = [
                rr
                for raw in c_srcs
                for rr in (_to_repo_rel(raw),)
                if rr and os.path.isfile(os.path.join(workdir, rr))
            ]
            std_roots = [
                rr for rr in norm_srcs if os.path.basename(rr) in std_names
            ]
            if std_roots:
                c_seeds = std_roots
            else:
                prod_mods, all_mods = set(), set()
                for s_rs in norm_srcs:
                    try:
                        with open(
                            os.path.join(workdir, s_rs),
                            encoding="utf-8",
                            errors="ignore",
                        ) as sf:
                            s_txt = sf.read()
                    except OSError:
                        continue
                    for m_nm, ex_p in extract_rust_submodules(
                        s_txt, include_cfg_test=True
                    ):
                        for c in _cands_for_mod(s_rs, m_nm, ex_p):
                            if os.path.isfile(os.path.join(workdir, c)):
                                all_mods.add(c)
                    for m_nm, ex_p in extract_rust_submodules(
                        s_txt, include_cfg_test=False
                    ):
                        for c in _cands_for_mod(s_rs, m_nm, ex_p):
                            if os.path.isfile(os.path.join(workdir, c)):
                                prod_mods.add(c)
                cfg_test_only_mods = all_mods - prod_mods
                c_seeds = [
                    rr for rr in norm_srcs if rr not in cfg_test_only_mods
                ]
        if not c_seeds:
            for cand_root in default_cands:
                rr = (
                    f"{clean_pkg}/{cand_root}".strip("/")
                    if clean_pkg
                    else cand_root
                )
                if os.path.isfile(os.path.join(workdir, rr)):
                    c_seeds.append(rr)
                    break
        return c_seeds

    seeds = []
    for spec in crate_specs:
        for rr in _resolve_crate_seeds(*spec):
            if rr not in seeds:
                seeds.append(rr)
    if not seeds:
        return False

    def _has_own_build(rel_dir):
        return (
            os.path.isfile(os.path.join(workdir, rel_dir, "BUILD.bazel"))
            or os.path.isfile(os.path.join(workdir, rel_dir, "BUILD.gn"))
            or os.path.isfile(os.path.join(workdir, rel_dir, "BUILD"))
        )

    def _same_pkg(rel_rs):
        cur = os.path.dirname(rel_rs).strip("/")
        while (
            cur
            and cur != clean_pkg
            and (not clean_pkg or cur.startswith(clean_pkg + "/"))
        ):
            if _has_own_build(cur):
                return False
            cur = os.path.dirname(cur).strip("/")
        return cur == clean_pkg

    inc_pat = re.compile(
        r"include(?:_(?:str|bytes))?!\s*\([^)]*" + re.escape(base_name), re.S
    )
    seen = set(seeds)
    queue = list(seeds)
    while queue:
        cur_rs = queue.pop(0)
        try:
            with open(
                os.path.join(workdir, cur_rs), encoding="utf-8", errors="ignore"
            ) as rf:
                rs_txt = rf.read()
        except OSError:
            continue
        prod_code = strip_rust_cfg_test(rs_txt)
        if inc_pat.search(prod_code) or (
            "concat!" in prod_code
            and re.search(r"include(?:_(?:str|bytes))?!", prod_code)
            and base_name in prod_code
        ):
            return True
        for mod_name, explicit_paths in extract_rust_submodules(
            rs_txt, include_cfg_test=False
        ):
            for cand in _cands_for_mod(
                cur_rs, mod_name, explicit_paths, is_seed=(cur_rs in seeds)
            ):
                if (
                    cand not in seen
                    and not cand.startswith("..")
                    and _same_pkg(cand)
                    and os.path.isfile(os.path.join(workdir, cand))
                ):
                    seen.add(cand)
                    queue.append(cand)
    return False


GENERIC_RUST_CALLABLE_SYMS = {
    "build",
    "create",
    "default",
    "from",
    "get",
    "init",
    "into",
    "load",
    "main",
    "new",
    "open",
    "parse",
    "read",
    "run",
    "set",
    "setup",
    "start",
    "test",
    "try_from",
    "try_into",
    "update",
    "write",
}

_RS_CONST_OR_STATIC_RE = re.compile(
    r"\b(?:const|static)\s+([A-Za-z_]\w*)\b([^;]*);"
)
_RS_FN_SIG_RE = re.compile(r"\bfn\s+([A-Za-z_]\w*)\b")
_RS_SELF_SKIP_WORDS = {
    "mut",
    "const",
    "dyn",
    "impl",
    "unsafe",
    "extern",
    "fn",
    "crate",
    "super",
    "self",
}


def extract_rust_impl_spans(clean):
    """Returns `(open_brace, close_brace, self_type_ident)` for `impl` blocks in comment-stripped Rust `clean`."""
    if not clean:
        return []
    n = len(clean)
    spans = []

    def _strip_leading_angles(s):
        idx = 0
        sn = len(s)
        while idx < sn and s[idx].isspace():
            idx += 1
        if idx >= sn or s[idx] != "<":
            return s[idx:]
        ad = pd = bd = 0
        k = idx
        while k < sn:
            lit = _skip_rust_string_or_char(s, k)
            if lit > k:
                k = lit
                continue
            if s.startswith(("->", "=>"), k):
                k += 2
                continue
            ch = s[k]
            if ch == "(":
                pd += 1
            elif ch == ")" and pd > 0:
                pd -= 1
            elif ch == "[":
                bd += 1
            elif ch == "]" and bd > 0:
                bd -= 1
            elif pd == 0 and bd == 0:
                if ch == "<":
                    ad += 1
                elif ch == ">" and ad > 0:
                    ad -= 1
                    if ad == 0:
                        return s[k + 1 :]
            k += 1
        return ""

    for im in re.finditer(r"\bimpl\b", clean):
        p = im.start()
        while True:
            while p > 0 and clean[p - 1].isspace():
                p -= 1
            m_prev = re.search(r"\b(?:unsafe|default)$", clean[:p])
            if m_prev:
                p = m_prev.start()
                continue
            break
        if p > 0 and clean[p - 1] not in ";{}]":
            continue
        angle_d = paren_d = bracket_d = 0
        open_b = -1
        k = im.end()
        while k < n:
            lit = _skip_rust_string_or_char(clean, k)
            if lit > k:
                k = lit
                continue
            if clean.startswith(("->", "=>"), k):
                k += 2
                continue
            ch = clean[k]
            if ch == "(":
                paren_d += 1
            elif ch == ")" and paren_d > 0:
                paren_d -= 1
            elif ch == "[":
                bracket_d += 1
            elif ch == "]" and bracket_d > 0:
                bracket_d -= 1
            elif paren_d == 0 and bracket_d == 0:
                if ch == "<":
                    angle_d += 1
                elif ch == ">" and angle_d > 0:
                    angle_d -= 1
                elif angle_d == 0:
                    if ch == ";":
                        break
                    if ch == "{":
                        open_b = k
                        break
            k += 1
        if open_b < 0:
            continue
        close_b = _find_rust_bracket_close(clean, open_b, "{", "}")
        hdr = _strip_leading_angles(clean[im.end() : open_b])
        if not hdr:
            continue
        ad = pd = bd = 0
        where_pos = None
        for_end = None
        hk, hn = 0, len(hdr)
        while hk < hn:
            lit = _skip_rust_string_or_char(hdr, hk)
            if lit > hk:
                hk = lit
                continue
            if hdr.startswith(("->", "=>"), hk):
                hk += 2
                continue
            ch = hdr[hk]
            if ch == "(":
                pd += 1
            elif ch == ")" and pd > 0:
                pd -= 1
            elif ch == "[":
                bd += 1
            elif ch == "]" and bd > 0:
                bd -= 1
            elif pd == 0 and bd == 0:
                if ch == "<":
                    ad += 1
                elif ch == ">" and ad > 0:
                    ad -= 1
                elif ad == 0 and (
                    hk == 0 or not (hdr[hk - 1].isalnum() or hdr[hk - 1] == "_")
                ):
                    if re.match(r"where\b", hdr[hk:]):
                        where_pos = hk
                        break
                    if re.match(r"for\b(?!\s*<)", hdr[hk:]):
                        for_end = hk + 3
            hk += 1
        if where_pos is not None:
            hdr = hdr[:where_pos]
            if for_end is not None and for_end > where_pos:
                for_end = None
        self_seg = hdr[for_end:] if for_end is not None else hdr
        stripped_chars = []
        ad = bd = 0
        in_arr_len = False
        sk, sn = 0, len(self_seg)
        while sk < sn:
            lit = _skip_rust_string_or_char(self_seg, sk)
            if lit > sk:
                sk = lit
                continue
            if self_seg.startswith(("->", "=>"), sk):
                sk += 2
                continue
            ch = self_seg[sk]
            if ad == 0 and ch == "[":
                bd += 1
                in_arr_len = False
            elif ad == 0 and ch == "]" and bd > 0:
                bd -= 1
                in_arr_len = False
            elif ad == 0 and bd > 0 and ch == ";":
                in_arr_len = True
            elif ch == "<":
                ad += 1
            elif ch == ">" and ad > 0:
                ad -= 1
            elif ad == 0 and not in_arr_len:
                stripped_chars.append(ch)
            sk += 1
        no_lt = re.sub(r"'[A-Za-z_]\w*", " ", "".join(stripped_chars))
        idents = [
            tok
            for tok in re.findall(r"[A-Za-z_]\w*", no_lt)
            if tok not in _RS_SELF_SKIP_WORDS
        ]
        if idents:
            spans.append((open_b, close_b, idents[-1]))
    return spans


def _rust_fn_body_span(clean, pos):
    paren_d = bracket_d = angle_d = 0
    i, n = pos, len(clean)
    while i < n:
        lit = _skip_rust_string_or_char(clean, i)
        if lit > i:
            i = lit
            continue
        if clean.startswith(("->", "=>"), i):
            i += 2
            continue
        ch = clean[i]
        if ch == "(":
            paren_d += 1
        elif ch == ")" and paren_d > 0:
            paren_d -= 1
        elif ch == "[":
            bracket_d += 1
        elif ch == "]" and bracket_d > 0:
            bracket_d -= 1
        elif paren_d == 0 and bracket_d == 0:
            if ch == "<":
                angle_d += 1
            elif ch == ">" and angle_d > 0:
                angle_d -= 1
            elif angle_d == 0:
                if ch == ";":
                    return None
                if ch == "{":
                    close_b = _find_rust_bracket_close(clean, i, "{", "}")
                    return i, close_b + 1
        i += 1
    return None


def _load_pkg_rust_files_and_defs(workdir, pkg, rs_cache):
    files_key = ("files", pkg)
    defs_key = ("defs", pkg)
    if defs_key in rs_cache:
        return rs_cache[files_key], rs_cache[defs_key]
    files = []
    root = os.path.join(workdir, pkg)
    if os.path.isdir(root):
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [
                dn
                for dn in dirnames
                if not dn.startswith(".")
                and not os.path.isfile(os.path.join(dirpath, dn, "BUILD.bazel"))
                and not os.path.isfile(os.path.join(dirpath, dn, "BUILD"))
            ]
            for fn in sorted(filenames):
                if not fn.endswith(".rs"):
                    continue
                fpath = os.path.join(dirpath, fn)
                try:
                    with open(fpath, encoding="utf-8", errors="ignore") as rf:
                        raw = rf.read()
                except OSError:
                    continue
                files.append(
                    (
                        os.path.relpath(fpath, root),
                        strip_rust_comments(raw, blank_strings=False),
                    )
                )
    crate_qual = os.path.basename(pkg.strip("/")).replace("-", "_") or None
    defs = []
    for rel_path, clean in files:
        stem = os.path.splitext(os.path.basename(rel_path))[0]
        mod_qual = stem if stem not in ("lib", "main", "mod") else crate_qual
        impl_spans = extract_rust_impl_spans(clean)
        for cm in _RS_CONST_OR_STATIC_RE.finditer(clean):
            defs.append((cm.group(1), mod_qual, cm.group(2)))
        for fm in _RS_FN_SIG_RE.finditer(clean):
            span = _rust_fn_body_span(clean, fm.end())
            if not span:
                continue
            b_start, b_end = span
            impl_ty = next(
                (ty for s, e, ty in impl_spans if s < fm.start() < e), None
            )
            defs.append(
                (fm.group(1), impl_ty or mod_qual, clean[b_start:b_end])
            )
    rs_cache[files_key] = files
    rs_cache[defs_key] = defs
    return files, defs


def _has_staged_leaf_token(text, leaf_tok):
    if not text or not leaf_tok:
        return False
    return bool(
        re.search(r"(?<![\w.-])" + re.escape(leaf_tok) + r"(?![\w.-])", text)
    )


def _matches_any_rust_callable(caller_clean, callables):
    for ident, qual in callables:
        if not re.search(r"\b" + re.escape(ident) + r"\b", caller_clean):
            continue
        if ident in GENERIC_RUST_CALLABLE_SYMS:
            if qual and re.search(
                r"\b" + re.escape(qual) + r"\b", caller_clean
            ):
                return True
            continue
        return True
    return False


def _pkg_consuming_rust_callables(
    workdir,
    pkg,
    tokens,
    rs_cache,
    downstream_syms=None,
    leaf=None,
    other_leaves=None,
):
    rs_files, defs = _load_pkg_rust_files_and_defs(workdir, pkg, rs_cache)
    if not rs_files:
        return []
    if not downstream_syms and not any(
        tok in clean for _, clean in rs_files for tok in tokens
    ):
        return []
    active_idx = set()
    active_non_generic = set()

    def _targets_other_sibling(body):
        return bool(
            leaf
            and other_leaves
            and not _has_staged_leaf_token(body, leaf)
            and any(_has_staged_leaf_token(body, ol) for ol in other_leaves)
        )

    for idx, (ident, _qual, body) in enumerate(defs):
        if _targets_other_sibling(body):
            continue
        hit = any(tok in body for tok in tokens) or (
            bool(downstream_syms)
            and _matches_any_rust_callable(body, downstream_syms)
        )
        if hit:
            active_idx.add(idx)
            if ident not in GENERIC_RUST_CALLABLE_SYMS:
                active_non_generic.add(ident)
    checked_syms = set()
    while active_non_generic - checked_syms:
        new_syms = sorted(active_non_generic - checked_syms)
        checked_syms.update(new_syms)
        pat = re.compile(
            r"\b(?:" + "|".join(re.escape(s) for s in new_syms) + r")\b"
        )
        for idx, (ident, _qual, body) in enumerate(defs):
            if idx in active_idx or _targets_other_sibling(body):
                continue
            if pat.search(body):
                active_idx.add(idx)
                if ident not in GENERIC_RUST_CALLABLE_SYMS:
                    active_non_generic.add(ident)
    seen = set()
    out = []
    for idx in sorted(active_idx):
        pair = (defs[idx][0], defs[idx][1])
        if pair not in seen:
            seen.add(pair)
            out.append(pair)
    return out


def is_host_test_data_consumed_via_rust_deps(
    workdir,
    caller_clean,
    direct_pkgs,
    hop2_by_pkg,
    tokens,
    leaf=None,
    other_leaves=None,
    rs_cache=None,
):
    """True when `caller_clean` consumes `tokens` via helpers/consts in 1- or 2-hop Rust deps."""
    if not caller_clean or not direct_pkgs or not tokens:
        return False
    cache = rs_cache if rs_cache is not None else {}
    eff_tokens = set(tokens)
    if leaf and other_leaves:
        if not _has_staged_leaf_token(caller_clean, leaf) and any(
            _has_staged_leaf_token(caller_clean, ol) for ol in other_leaves
        ):
            eff_tokens = {t for t in eff_tokens if t == leaf}
            if not eff_tokens:
                return False
    hop2_callables = {}
    for dpkg in direct_pkgs:
        direct_callables = _pkg_consuming_rust_callables(
            workdir,
            dpkg,
            eff_tokens,
            cache,
            leaf=leaf,
            other_leaves=other_leaves,
        )
        if direct_callables and _matches_any_rust_callable(
            caller_clean, direct_callables
        ):
            return True
        hop2_syms = []
        for hpkg in sorted((hop2_by_pkg or {}).get(dpkg) or ()):
            if hpkg not in hop2_callables:
                hop2_callables[hpkg] = _pkg_consuming_rust_callables(
                    workdir,
                    hpkg,
                    eff_tokens,
                    cache,
                    leaf=leaf,
                    other_leaves=other_leaves,
                )
            hop2_syms.extend(hop2_callables[hpkg])
        if hop2_syms:
            wrapped = _pkg_consuming_rust_callables(
                workdir,
                dpkg,
                eff_tokens,
                cache,
                downstream_syms=hop2_syms,
                leaf=leaf,
                other_leaves=other_leaves,
            )
            if wrapped and _matches_any_rust_callable(caller_clean, wrapped):
                return True
            dpkg_files, _ = _load_pkg_rust_files_and_defs(workdir, dpkg, cache)
            if any(
                "pub use " in clean for _, clean in dpkg_files
            ) and _matches_any_rust_callable(caller_clean, hop2_syms):
                return True
    return False
