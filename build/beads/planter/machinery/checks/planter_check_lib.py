#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared helpers for Planter deterministic check scripts and tools."""

import ast
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


def _resolve_active_target_dirs(workdir, target_dirs=None):
    """Normalizes explicit `target_dirs` or `$PLANTER_TARGET_DIRS` / `$PLANTER_TARGET_DIR`."""
    if target_dirs is None:
        target_dirs = (
            os.environ.get("PLANTER_TARGET_DIRS", "").strip()
            or os.environ.get("PLANTER_TARGET_DIR", "").strip()
        )
    return parse_target_dirs(target_dirs, workdir)


def resolve_change_base(workdir, change_base=None, target_dirs=None):
    """Deterministically resolves the git change base for the current check run."""
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
        base = merge_base or ("HEAD~1" if parent_sha else base)
        base_sha = _git_out(
            workdir, ["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"]
        )

    # Do not step back to HEAD~1 when HEAD is already merged on origin/main
    # (e.g. an upstream [bazel_migration] commit at checkout base while edits
    # are uncommitted) or is a stacked prerequisite commit outside target_dirs.
    head_on_origin = bool(
        origin_main
        and head_sha
        and (head_sha == origin_main or merge_base == head_sha)
    )
    if parent_sha and not head_on_origin:
        subj = _git_out(workdir, ["log", "-1", "--format=%s", "HEAD"])
        head_msg = _git_out(workdir, ["log", "-1", "--format=%B", "HEAD"])
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
        tdirs = _resolve_active_target_dirs(workdir, target_dirs)
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
        # HEAD is not on origin/main here, so a HEAD commit touching target_dirs
        # is a local task commit even without a migration subject or footer.
        is_local_task_head = is_mig_subj or (
            head_touches_tdirs and bool(origin_main)
        )
        if is_local_task_head and (base == "HEAD" or base_sha == head_sha):
            active_tdirs = [
                td
                for td in tdirs
                if os.path.exists(os.path.join(workdir, td))
                or any(f == td or f.startswith(td + "/") for f in head_files)
            ]
            if not active_tdirs or head_touches_tdirs:
                return "HEAD~1"
            return base
        if (
            is_local_task_head
            and head_touches_tdirs
            and base_sha
            and base_sha != parent_sha
            and _is_ancestor(workdir, base_sha, parent_sha)
        ):
            head_cids = re.findall(r"(?mi)^Change-Id:\s*(I[0-9a-f]+)", head_msg)
            parent_msg = _git_out(
                workdir, ["log", "-1", "--format=%B", "HEAD~1"]
            )
            parent_cids = re.findall(
                r"(?mi)^Change-Id:\s*(I[0-9a-f]+)", parent_msg
            )
            if head_cids and parent_cids and head_cids[-1] != parent_cids[-1]:
                return "HEAD~1"
            stack_out = _git_out(
                workdir, ["diff", "--name-only", base_sha, "HEAD~1"]
            )
            stack_files = [
                ln.strip() for ln in stack_out.splitlines() if ln.strip()
            ]
            if not any(
                any(p == td or p.startswith(td + "/") for td in tdirs)
                for p in stack_files
            ):
                return "HEAD~1"

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
        return [
            line.strip()
            for line in os.environ["PLANTER_CHANGED_FILES"].splitlines()
            if line.strip()
        ]

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
        mod_txt = open(mod_path, encoding="utf-8").read()
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
                mtxt = open(mfile, encoding="utf-8").read()
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
                    btxt = open(
                        os.path.join(cur_root, "BUILD.bazel"), encoding="utf-8"
                    ).read()
                except OSError:
                    continue
                if "@fuchsia_sdk" in btxt or "//sdk/fidl/" in btxt:
                    continue
                for srel in re.findall(r'"([^"]+\.(?:cc|cpp|c|h))"', btxt):
                    sp = os.path.join(cur_root, srel)
                    if not os.path.isfile(sp):
                        continue
                    try:
                        stxt = open(
                            sp, encoding="utf-8", errors="ignore"
                        ).read()
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
