#!/bin/bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Deterministic check mirroring the CQ static-checks `gn_format` and
# `verify_bazel2gn` GN-formatter version requirements:
# 1. Verifies that the local `prebuilt/third_party/gn/<platform>/gn` binary
#    matches the `gn/gn/<platform>` `git_revision` pinned in `manifests/jiri.lock`
#    or `manifests/toolchain` (catching checkouts where `git rebase origin/main`
#    advanced the manifest across a GN roll without updating gitignored CIPD
#    prebuilts, which causes local `fx bazel2gn` and `shac check` to format
#    `BUILD.gn` with the stale GN binary and fail `gn_format` + `verify_bazel2gn`
#    in CQ).
# 2. Runs `gn format --dry-run` (using the manifest-pinned GN version via a
#    `/tmp` CIPD cache when the local prebuilt is stale) on every GN file
#    (`BUILD.gn`, `*.gni`, `*.gn`) added or modified by the change.
# Prints a JSON array of findings on stdout.

WORKDIR="${PLANTER_WORKDIR:-.}"
CHANGE_BASE="${PLANTER_CHANGE_BASE:-HEAD}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

python3 - "$WORKDIR" "$CHANGE_BASE" "$TARGET_DIRS" <<'PYEOF'
import json
import os
import re
import shutil
import subprocess
import sys

workdir = os.path.abspath(sys.argv[1])
change_base = sys.argv[2].strip() or "HEAD"
raw_target_dirs = sys.argv[3]

SENTINEL = "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##"

# Map local prebuilt subdirectories to CIPD package names in manifests/jiri.lock.
PREBUILT_CANDIDATES = (
    ("prebuilt/third_party/gn/linux-x64/gn", "gn/gn/linux-amd64"),
    ("prebuilt/third_party/gn/linux-arm64/gn", "gn/gn/linux-arm64"),
    ("prebuilt/third_party/gn/mac-x64/gn", "gn/gn/mac-amd64"),
    ("prebuilt/third_party/gn/mac-arm64/gn", "gn/gn/mac-arm64"),
)


# Collect modified and untracked GN files across the change.
def git_lines(args):
    try:
        out = subprocess.check_output(
            ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=True
        )
    except Exception:
        return []
    return [l.strip() for l in out.splitlines() if l.strip()]


def collect_changed_gn_files():
    changed = set(git_lines(["diff", "--name-only", change_base]))
    changed.update(git_lines(["ls-files", "--others", "--exclude-standard"]))
    if change_base == "HEAD" and not any(
        p.endswith(".gn") or p.endswith(".gni") for p in changed
    ):
        changed.update(git_lines(["diff", "--name-only", "HEAD~1", "HEAD"]))
    for raw_dir in re.split(r"[\s,]+", raw_target_dirs):
        d = raw_dir.strip().strip("/")
        if d:
            changed.add(os.path.join(d, "BUILD.gn"))
    return sorted(
        p for p in changed
        if (p.endswith(".gn") or p.endswith(".gni"))
        and os.path.isfile(os.path.join(workdir, p))
    )


# Resolve the local GN prebuilt binary and its CIPD package name.
def find_local_gn():
    for rel_path, cipd_pkg in PREBUILT_CANDIDATES:
        abs_path = os.path.join(workdir, rel_path)
        if os.path.isfile(abs_path):
            return abs_path, rel_path, cipd_pkg
    which_gn = shutil.which("gn")
    return which_gn, "gn", None


# Parse the gn/gn/<platform> git_revision from jiri.lock JSON text or toolchain XML text.
def parse_gn_version_from_lock_text(lock_text, cipd_pkg):
    if not lock_text or not cipd_pkg:
        return None, None
    try:
        entries = json.loads(lock_text)
        for entry in entries:
            if isinstance(entry, dict) and entry.get("package") == cipd_pkg:
                ver = str(entry.get("version", "")).strip()
                if ver.startswith("git_revision:"):
                    return ver, ver.split(":", 1)[1]
    except Exception:
        pass
    return None, None


def parse_gn_version_from_toolchain_text(toolchain_text):
    if not toolchain_text:
        return None, None
    m = re.search(
        r'name="gn/gn/\$\{platform\}"\s+version="(git_revision:([0-9a-f]{12,40}))"',
        toolchain_text,
    )
    return (m.group(1), m.group(2)) if m else (None, None)


# Read the pinned gn/gn/<platform> git_revision from manifests/jiri.lock or manifests/toolchain,
# preferring origin/main's version when origin/main has advanced across a GN roll.
def read_pinned_gn_version(cipd_pkg):
    if not cipd_pkg:
        return None, None, False
    wt_ver, wt_rev = None, None
    lock_path = os.path.join(workdir, "manifests", "jiri.lock")
    if os.path.isfile(lock_path):
        try:
            with open(lock_path, "r", encoding="utf-8") as f:
                wt_ver, wt_rev = parse_gn_version_from_lock_text(f.read(), cipd_pkg)
        except Exception:
            pass
    if not wt_rev:
        toolchain_path = os.path.join(workdir, "manifests", "toolchain")
        if os.path.isfile(toolchain_path):
            try:
                with open(toolchain_path, "r", encoding="utf-8") as f:
                    wt_ver, wt_rev = parse_gn_version_from_toolchain_text(f.read())
            except Exception:
                pass

    # Check if origin/main is ahead of HEAD and rolled gn/gn/<platform> in manifests/jiri.lock.
    try:
        anc = subprocess.run(
            ["git", "-C", workdir, "merge-base", "--is-ancestor", "origin/main", "HEAD"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=5,
        )
        if anc.returncode != 0:
            up_lock = subprocess.check_output(
                ["git", "-C", workdir, "show", "origin/main:manifests/jiri.lock"],
                stderr=subprocess.DEVNULL,
                text=True,
                timeout=5,
            )
            up_ver, up_rev = parse_gn_version_from_lock_text(up_lock, cipd_pkg)
            if not up_rev:
                up_tc = subprocess.check_output(
                    ["git", "-C", workdir, "show", "origin/main:manifests/toolchain"],
                    stderr=subprocess.DEVNULL,
                    text=True,
                    timeout=5,
                )
                up_ver, up_rev = parse_gn_version_from_toolchain_text(up_tc)
            if up_rev and up_rev != wt_rev:
                return up_ver, up_rev, True
    except Exception:
        pass

    return wt_ver, wt_rev, False


# Inspect `gn --version` output (e.g. `2614 (ed7fee04cb30)`) for the short git revision.
def get_gn_binary_short_rev(gn_bin_path):
    if not gn_bin_path:
        return None
    try:
        out = subprocess.check_output(
            [gn_bin_path, "--version"], stderr=subprocess.DEVNULL, text=True, timeout=10
        ).strip()
    except Exception:
        return None
    m = re.search(r"\(([0-9a-f]{7,40})\)", out)
    return m.group(1) if m else None


# Export the manifest-pinned GN binary into a /tmp cache when the local prebuilt is stale.
def ensure_pinned_gn_in_tmp(cipd_pkg, pinned_version, pinned_rev):
    cache_dir = f"/tmp/planter-cipd-gn-{pinned_rev[:12]}"
    cached_gn = os.path.join(cache_dir, "gn")
    if os.path.isfile(cached_gn) and os.access(cached_gn, os.X_OK):
        return cached_gn
    cipd_bin = os.path.join(workdir, ".jiri_root", "bin", "cipd")
    if not os.path.isfile(cipd_bin):
        cipd_bin = shutil.which("cipd")
    if not cipd_bin:
        return None
    try:
        os.makedirs(cache_dir, exist_ok=True)
        subprocess.run(
            [cipd_bin, "export", "-root", cache_dir, "-ensure-file", "-"],
            input=f"{cipd_pkg} {pinned_version}\n",
            capture_output=True,
            text=True,
            timeout=60,
            check=True,
        )
    except Exception:
        return None
    return cached_gn if os.path.isfile(cached_gn) else None


# Check whether a GN file is managed by bazel2gn via the sentinel comment.
def has_bazel2gn_sentinel(rel_path):
    try:
        with open(os.path.join(workdir, rel_path), "r", encoding="utf-8") as f:
            return SENTINEL in f.read()
    except Exception:
        return False


gn_files = collect_changed_gn_files()
gn_bin, gn_rel, cipd_pkg = find_local_gn()
pinned_version, pinned_rev, upstream_gn_roll = read_pinned_gn_version(cipd_pkg)
local_rev = get_gn_binary_short_rev(gn_bin)

stale_prebuilt = bool(
    gn_bin
    and cipd_pkg
    and pinned_rev
    and (upstream_gn_roll or not local_rev or not pinned_rev.startswith(local_rev))
)

check_gn_bin = gn_bin
if stale_prebuilt:
    cached = ensure_pinned_gn_in_tmp(cipd_pkg, pinned_version, pinned_rev)
    if cached:
        check_gn_bin = cached

sentinel_dirs = sorted(
    {
        os.path.dirname(p) or "."
        for p in gn_files
        if os.path.basename(p) == "BUILD.gn" and has_bazel2gn_sentinel(p)
    }
)
b2g_sync_cmd = (
    "fx bazel2gn " + " ".join(f"-d {d}" for d in sentinel_dirs)
    if sentinel_dirs
    else "fx bazel2gn -d <dir>"
)
gn_dir_rel = os.path.dirname(gn_rel) if gn_rel != "gn" else "prebuilt/third_party/gn/linux-x64"
cipd_update_cmd = (
    f'.jiri_root/bin/cipd export -root {gn_dir_rel} -ensure-file - '
    f'<<< "{cipd_pkg} {pinned_version}"'
    if (cipd_pkg and pinned_version)
    else ""
)

findings = []

# Report a stale local GN prebuilt binary (or unmerged origin/main GN roll) so local fx bazel2gn and shac match CQ.
if gn_files and stale_prebuilt:
    target_file = gn_files[0]
    rebase_prefix = "Run `git rebase origin/main`, update " if upstream_gn_roll else "Update "
    msg = (
        f"Local GN prebuilt '{gn_rel}' is at revision '{local_rev or 'unknown'}', "
        f"but manifests/jiri.lock (on {'origin/main' if upstream_gn_roll else 'HEAD'}) "
        f"pins '{pinned_version}'. Because `prebuilt/` is gitignored, `git rebase origin/main` "
        "does not update the GN binary; running `fx bazel2gn` or `fx lint` "
        "with the stale binary produces `BUILD.gn` files that fail `gn_format` and "
        "`verify_bazel2gn` in CQ."
    )
    rem = (
        f"{rebase_prefix}the local GN prebuilt in place to match manifests/jiri.lock by running "
        f"`{cipd_update_cmd}`, then re-sync bazel2gn directories with `{b2g_sync_cmd}` "
        f"and format any hand-edited GN files with `{gn_rel} format <file>`."
    )
    findings.append({
        "source": "gn_format",
        "category": "stale_gn_prebuilt",
        "severity": "error",
        "file": target_file,
        "line": 1,
        "message": msg,
        "remediation": rem,
    })

# Check each modified GN file with the manifest-pinned GN formatter.
if gn_files and check_gn_bin:
    for rel in gn_files:
        try:
            res = subprocess.run(
                [check_gn_bin, "format", "--dry-run", rel],
                cwd=workdir,
                capture_output=True,
                text=True,
                timeout=60,
            )
        except Exception:
            continue
        if res.returncode == 2:
            is_sentinel = os.path.basename(rel) == "BUILD.gn" and has_bazel2gn_sentinel(rel)
            rel_dir = os.path.dirname(rel) or "."
            if stale_prebuilt and cipd_update_cmd:
                rebase_step = "`git rebase origin/main` and " if upstream_gn_roll else ""
                rem = (
                    f"First run {rebase_step}`{cipd_update_cmd}` to update the stale local GN "
                    "prebuilt to match manifests/jiri.lock, then run "
                    + (
                        f"`fx bazel2gn -d {rel_dir}` (so both `gn_format` and "
                        "`verify_bazel2gn` are satisfied)."
                        if is_sentinel
                        else f"`{gn_rel} format {rel}`."
                    )
                )
            elif is_sentinel:
                rem = (
                    f"Run `fx bazel2gn -d {rel_dir}` (or `{gn_rel} format {rel}`) "
                    "so BUILD.gn is formatted and in sync with BUILD.bazel."
                )
            else:
                rem = (
                    f"Run `{gn_rel} format {rel}` (or `fx format-code` from the "
                    "repository root) after every hand edit or bazel2gn run; for "
                    "sorted label lists such as verification .gni files, insert new "
                    "entries in the position `gn format` expects."
                )
            findings.append({
                "source": "gn_format",
                "category": "gn_format",
                "severity": "error",
                "file": rel,
                "line": 1,
                "message": (
                    f"GN file '{rel}' is not formatted according to the manifest-pinned "
                    "`gn format` (CQ static-checks `gn_format` and `verify_bazel2gn` fail)."
                ),
                "remediation": rem,
            })
        elif res.returncode == 1:
            err_excerpt = (res.stderr or res.stdout).strip()[:300]
            findings.append({
                "source": "gn_format",
                "category": "gn_syntax_error",
                "severity": "error",
                "file": rel,
                "line": 1,
                "message": f"`gn format` could not parse '{rel}': {err_excerpt}",
                "remediation": (
                    f"Fix the GN syntax error in '{rel}', then run `{gn_rel} format {rel}`."
                ),
            })

print(json.dumps(findings, indent=2))
PYEOF
