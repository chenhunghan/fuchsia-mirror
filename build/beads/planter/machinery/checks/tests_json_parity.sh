#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

# Deterministic check that the change preserves every test and test execution
# attribute of <build dir>/tests.json across GN-to-Bazel migrations.
#
# Key guarantees (implemented with verify_tests_json_parity.py):
# 1. Primary Product-Root Pass: Runs baseline and post-change `fx gen` against
#    the true CQ product roots (`//bundles/buildbot/core` and
#    `//bundles/buildbot/host`, preserving board/product from `args.gn`) WITHOUT
#    injecting synthetic per-directory `//<dir>:tests` roots, so severing an
#    ancestor `group("tests")` edge immediately drops the test from `tests.json`.
# 2. Off-CQ Leaf Discovery Pass: Any test target in a touched baseline
#    `BUILD.gn` absent from the primary product `tests.json` is traced upward
#    via `fx gn refs` (with static `BUILD.gn` group fallback) to its highest
#    enclosing `group(...)` root and checked in a secondary scoped `fx gen` pass,
#    preventing `0 == 0` blind spots on leaf packages without local `group("tests")`.
# 3. Dual-Build `-gn` vs. `gn2gone` State Machine: Distinguishes Case 1 full GN
#    test removal (`1 gn -> 1 bazel`), Case 2 dual-build (`1 gn -> 1 bazel +
#    1 gn_dual` unless attested via `Skipped-Dual-Tests:`), and Case 3 `gn2gone`
#    sentinel retirement (`1 bazel + 1 gn_dual -> 1 bazel` with environments
#    preserved).
# 4. Attribute Parity & Fail-Closed `fx gen`: Verifies `os`, `cpu`, normalized
#    `environments` (dimensions/tags), `log_settings.max_severity`, and
#    `build_only` (`false -> true` is an ERROR). Any `fx gen` failure reports
#    `tests_json_parity_fx_gen_failed` as ERROR (never skipped as INFO).
# 5. Bijective `Renamed-Tests: <old> -> <new>` and 1:1 same-package host wrapper
#    matching; remediation forbids waiving dropped tests via `disputed_findings`
#    or inventing `Deleted-Tests:`.
#
# Environment:
#   PLANTER_SKIP_BUILD=1           skip the check (reports an INFO finding).
#   PLANTER_BUILD_DRY_RUN=1        run nothing; report the plan as an INFO finding.
#   PLANTER_GEN_TIMEOUT=<secs>     per-`fx gen` timeout (default 1800).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKDIR="${PLANTER_WORKDIR:-.}"
TARGET_DIR="${PLANTER_TARGET_DIR:-}"
TARGET_DIRS="${PLANTER_TARGET_DIRS:-$TARGET_DIR}"

first_dir="${TARGET_DIRS%% *}"
if [[ -n "$TARGET_DIR" && -n "$first_dir" && "$TARGET_DIR" != "$first_dir" ]]; then
  echo "[]"
  exit 0
fi

# exec, so that whoever stops this check signals the process that edits the
# working tree, and its `finally` restores it.
exec python3 - "$WORKDIR" "$TARGET_DIRS" "$SCRIPT_DIR" <<'PYEOF'
import base64
import fcntl
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

workdir = os.path.abspath(sys.argv[1])
target_dirs = [
    d.strip().strip("/")
    for d in re.split(r"[\s,]+", sys.argv[2])
    if d.strip().strip("/")
]
script_dir = os.path.abspath(sys.argv[3])
sys.path.insert(0, script_dir)

import verify_tests_json_parity as vtp  # noqa: E402

change_base = (
    os.environ.get("PLANTER_RESOLVED_CHANGE_BASE", "").strip()
    or os.environ.get("PLANTER_CHANGE_BASE", "").strip()
    or "HEAD"
)
timeout = int(os.environ.get("PLANTER_GEN_TIMEOUT", "1800"))
dry_run = os.environ.get("PLANTER_BUILD_DRY_RUN", "").strip() not in ("", "0")
skip_build = os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
project_dir = os.path.realpath(
    os.environ.get("PLANTER_PROJECT_DIR", "") or os.devnull
)

SOURCE = vtp.SOURCE
# Bump when the cached baseline format or test normalization changes.
BASELINE_VERSION = "3"
# A change touching more files than this is not a migration on top of
# $PLANTER_CHANGE_BASE (e.g. a stale base); reverting it would be slow and risky.
MAX_REVERTED_PATHS = 2000
# Untracked files outside the target directories only matter to `fx gen` if
# they are build files.
BUILD_FILE = re.compile(
    r"(^|/)(BUILD|BUILD\.gn|BUILD\.bazel|MODULE\.bazel|[^/]+\.gni|[^/]+\.bzl)$"
)
ROOTS_MARKER = vtp.ROOTS_MARKER
# Default `fx set` product roots, stripped when reconstructing args.gn from args.json.
PRODUCT_TARGET_ROOT = "//build/images/fuchsia"
PRODUCT_HOST_ROOT = "//:host"

FX_GEN_REMEDIATION = (
    "Fix the GN or Bazel generation failure so `fx gen` succeeds and "
    "`<build dir>/tests.json` can be verified (`tests_json_parity` fails "
    "closed on `fx gen` errors and cannot be waived via `disputed_findings`)."
)

findings = []


def add(category, severity, message, remediation=None, file=None):
    findings.append(
        vtp.make_finding(
            category=category,
            severity=severity,
            message=message,
            remediation=remediation,
            file=file,
        )
    )


def finish():
    print(json.dumps(findings, indent=2))
    sys.exit(0)


# Turning SIGTERM into SystemExit runs the `finally` that restores the tree.
for sig in (signal.SIGTERM, signal.SIGHUP):
    signal.signal(sig, lambda signum, frame: sys.exit(128 + signum))


def git(args, text=True):
    return subprocess.check_output(
        ["git", "-C", workdir] + args, stderr=subprocess.DEVNULL, text=text
    )


# Advance change_base when HEAD was rebased onto a newer origin/main after
# PLANTER_CHANGE_BASE was pinned, so baseline revert only touches the change's files.
def resolve_effective_change_base(raw_base):
    def _git_str(cmd_args):
        try:
            return git(cmd_args).strip()
        except (OSError, subprocess.CalledProcessError):
            return ""

    def _is_anc(anc_ref, desc_ref):
        if not anc_ref or not desc_ref:
            return False
        return (
            subprocess.run(
                ["git", "-C", workdir, "merge-base", "--is-ancestor", anc_ref, desc_ref],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            ).returncode
            == 0
        )

    base = (raw_base or "").strip() or "HEAD"
    head_sha = _git_str(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
    base_sha = _git_str(["rev-parse", "--verify", "--quiet", f"{base}^{{commit}}"])
    origin_main = _git_str(
        ["rev-parse", "--verify", "--quiet", "origin/main^{commit}"]
    )
    merge_base = (
        _git_str(["merge-base", origin_main, head_sha])
        if (origin_main and head_sha)
        else ""
    )
    if (
        base_sha
        and merge_base
        and base_sha != merge_base
        and _is_anc(base_sha, merge_base)
    ):
        return merge_base
    if head_sha and base_sha and not _is_anc(base_sha, head_sha):
        return merge_base or "HEAD~1"
    return base


change_base = resolve_effective_change_base(change_base)


def read(rel):
    try:
        with open(
            os.path.join(workdir, rel), encoding="utf-8", errors="replace"
        ) as f:
            return f.read()
    except OSError:
        return None


def base_text(rel):
    try:
        return git(["show", f"{change_base}:{rel}"])
    except (OSError, subprocess.CalledProcessError):
        return None


def sha(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    try:
        with open(path, "rb") as f:
            return sha(f.read())
    except OSError:
        return ""


def state(path):
    """What is at `path`, for telling backed-up, written and edited contents apart."""
    if os.path.islink(path):
        return "link:" + os.readlink(path)
    if os.path.isfile(path):
        return "file:" + file_digest(path)
    if os.path.isdir(path):
        return "dir"
    return "absent"


try:
    git_dir = git(["rev-parse", "--absolute-git-dir"]).strip()
except (OSError, subprocess.CalledProcessError):
    add(
        "tests_json_parity_skipped",
        "INFO",
        f"{workdir} is not a git checkout; tests.json parity was not checked.",
    )
    finish()

state_dir = os.path.join(git_dir, "planter-tests-json-parity")
journal_path = os.path.join(state_dir, "restore.json")
backup_dir = os.path.join(state_dir, "backup")
lock_path = os.path.join(state_dir, "lock")

# Serialize concurrent tests_json_parity invocations across background and post-report
# check runs so an overlapping run never restores or deletes an active run's backup_dir.
os.makedirs(state_dir, exist_ok=True)
lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o644)
if skip_build:
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        add(
            "tests_json_parity_skipped",
            "INFO",
            "PLANTER_SKIP_BUILD is set; tests.json parity was not checked.",
        )
        finish()
else:
    fcntl.flock(lock_fd, fcntl.LOCK_EX)


def remove_file(path):
    if os.path.islink(path) or os.path.isfile(path):
        os.unlink(path)


def prune_empty_dirs(rel):
    """Removes directories left empty by deleting `rel`, up to the workdir."""
    d = os.path.dirname(os.path.join(workdir, rel))
    while d.startswith(workdir + os.sep):
        try:
            os.rmdir(d)
        except OSError:
            return
        d = os.path.dirname(d)


class RestoreConflict(Exception):
    pass


# Reconstruct `<build_dir>/args.gn` from `<build_dir>/args.json` when an interrupted
# prior run lost its backup copy of `args.gn`.
def reconstruct_args_gn_from_json(rel_build_dir, expected_state=""):
    args_json_path = os.path.join(workdir, rel_build_dir, "args.json")
    if not os.path.isfile(args_json_path):
        return None
    try:
        with open(args_json_path, encoding="utf-8") as f:
            data = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    board = data.get("build_info_board", "")
    product = data.get("build_info_product", "")
    imported_vars = set()
    imports = []
    for sub, name in (("boards", board), ("products", product)):
        if name and isinstance(name, str):
            rel_gni = f"{sub}/{name}.gni"
            gni_path = os.path.join(workdir, rel_gni)
            if os.path.isfile(gni_path):
                imports.append(f'import("//{rel_gni}")')
                try:
                    with open(gni_path, encoding="utf-8", errors="replace") as gf:
                        for m in re.finditer(
                            r"^\s*([A-Za-z_]\w*)\s*=", gf.read(), re.M
                        ):
                            imported_vars.add(m.group(1))
                except OSError:
                    pass
    target_list_keys = (
        "build_only_labels",
        "developer_test_labels",
        "e2e_test_labels",
        "hermetic_test_package_labels",
        "host_labels",
        "host_test_labels",
        "target_labels",
        "universe_package_labels",
    )
    fx_bin = next(
        (
            os.path.join(workdir, rel)
            for rel in (".jiri_root/bin/fx", "scripts/fx")
            if os.path.isfile(os.path.join(workdir, rel))
        ),
        None,
    )
    cands = []
    for drop_dev in (True, False):
        for strip_prod_roots in (True, False):
            for include_overrides in (False, True):
                lines = list(imports)
                if "compilation_mode" in data:
                    lines.append("\n# Compilation args:")
                    cm = json.dumps(data["compilation_mode"])
                    lines.append(f"compilation_mode = {cm}")
                basic = []
                for k, v in sorted(data.items()):
                    if (
                        k in imported_vars
                        or k == "compilation_mode"
                        or k in target_list_keys
                        or k == "product_assembly_overrides"
                    ):
                        continue
                    basic.append(f"{k} = {json.dumps(v)}")
                if basic:
                    lines.append("\n# Basic args:")
                    lines.extend(basic)
                tlists = []
                for k in sorted(target_list_keys):
                    if k not in data:
                        continue
                    if k == "developer_test_labels" and drop_dev:
                        continue
                    vals = list(data[k]) if isinstance(data[k], list) else []
                    if strip_prod_roots and k == "target_labels":
                        vals = [x for x in vals if x != PRODUCT_TARGET_ROOT]
                    if strip_prod_roots and k == "host_labels":
                        vals = [x for x in vals if x != PRODUCT_HOST_ROOT]
                    tlists.append(f"{k} = {json.dumps(vals)}")
                if tlists:
                    lines.append("\n# Target lists:")
                    lines.extend(tlists)
                if include_overrides:
                    ov = data.get("product_assembly_overrides", [])
                    lines.append(f"\nproduct_assembly_overrides = {json.dumps(ov)}")
                raw = "\n".join(lines) + "\n"
                cur_cands = [raw.encode("utf-8")]
                if fx_bin:
                    try:
                        fmt = subprocess.check_output(
                            [fx_bin, "gn", "format", "--stdin"],
                            cwd=workdir,
                            input=raw,
                            stderr=subprocess.DEVNULL,
                            text=True,
                        )
                        cur_cands.insert(0, fmt.encode("utf-8"))
                    except (OSError, subprocess.CalledProcessError):
                        pass
                for c in cur_cands:
                    cands.append(c)
                    if expected_state and ("file:" + sha(c)) == expected_state:
                        return c
    return cands[0] if cands else None


# Recover backup bytes for a journal entry when `backup_dir` is missing.
def recover_missing_backup_bytes(entry):
    rel = entry["path"]
    expected = entry.get("backup_state", "")
    b64 = entry.get("backup_b64")
    if isinstance(b64, str) and b64:
        try:
            return base64.b64decode(b64)
        except ValueError:
            pass
    dst = os.path.join(workdir, rel)
    if rel.endswith("/args.gn"):
        if os.path.isfile(dst):
            cur_txt = read(rel) or ""
            marker = f"\n{ROOTS_MARKER}"
            if marker in cur_txt:
                stripped = cur_txt.split(marker, 1)[0].encode("utf-8")
                if not expected or ("file:" + sha(stripped)) == expected:
                    return stripped
        reconstructed = reconstruct_args_gn_from_json(
            os.path.dirname(rel), expected
        )
        if reconstructed is not None:
            return reconstructed
    try:
        blob = git(["cat-file", "blob", f"HEAD:{rel}"], text=False)
        if not expected or ("file:" + sha(blob)) == expected:
            return blob
    except (OSError, subprocess.CalledProcessError):
        pass
    return None


def write_journal(entries):
    fd, tmp = tempfile.mkstemp(dir=state_dir, prefix=".restore.")
    with os.fdopen(fd, "w") as f:
        json.dump({"paths": entries}, f)
    os.replace(tmp, journal_path)


def back_up(entries):
    """Backs up the files of `entries` and writes the journal."""
    os.makedirs(state_dir, exist_ok=True)
    shutil.rmtree(backup_dir, ignore_errors=True)
    for e in entries:
        src = os.path.join(workdir, e["path"])
        e["backup_state"] = state(src)
        if e["backup_state"] != "absent":
            dst = os.path.join(backup_dir, e["path"])
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            shutil.copy2(src, dst, follow_symlinks=False)
            if os.path.isfile(src) and not os.path.islink(src):
                try:
                    if os.path.getsize(src) <= 2 * 1024 * 1024:
                        with open(src, "rb") as bf:
                            e["backup_b64"] = base64.b64encode(
                                bf.read()
                            ).decode("ascii")
                except OSError:
                    pass
    write_journal(entries)


def restore_from_journal():
    """Restores backed-up files; raises RestoreConflict if edited externally."""
    with open(journal_path, encoding="utf-8") as f:
        entries = json.load(f)["paths"]
    conflicts = []
    for e in entries:
        rel = e["path"]
        dst = os.path.join(workdir, rel)
        current = state(dst)
        backup_st = e.get("backup_state", "absent")
        written_sts = e.get("written_states") or []
        lost_during_restore = (
            current == "absent"
            and backup_st.startswith("file:")
            and "absent" not in written_sts
        )
        if current == backup_st or (
            current not in written_sts and not lost_during_restore
        ):
            # Already restored or superseded by a newer working-tree edit/rebase.
            continue
        if backup_st == "absent":
            remove_file(dst)
            prune_empty_dirs(rel)
        else:
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            bkp_src = os.path.join(backup_dir, rel)
            if os.path.lexists(bkp_src):
                shutil.copy2(bkp_src, dst, follow_symlinks=False)
            else:
                recovered = recover_missing_backup_bytes(e)
                if recovered is not None:
                    with open(dst, "wb") as wf:
                        wf.write(recovered)
                else:
                    conflicts.append(e)
    if conflicts:
        write_journal(conflicts)
        raise RestoreConflict(", ".join(e["path"] for e in conflicts))
    os.unlink(journal_path)
    shutil.rmtree(backup_dir, ignore_errors=True)


def report_restore_failure(error):
    add(
        "tests_json_parity_restore_failed",
        "ERROR",
        "tests_json_parity temporarily wrote the $PLANTER_CHANGE_BASE version "
        f"of changed files (and test roots into args.gn) and could not restore "
        f"all of them: {error}. The originals are in {backup_dir}, listed in "
        f"{journal_path}. A file whose content changed after the check wrote "
        "it is never overwritten.",
        remediation=(
            f"Compare each file listed in {journal_path} with its copy in "
            f"{backup_dir}, keep the right content, then delete both and run "
            "the checks again."
        ),
    )


# Serialize working-tree reversion and `fx gen` runs against build_verification
# so concurrent check invocations never collide in regenerator.py.
def acquire_workdir_build_lock(gdir):
    if (skip_build and not os.path.exists(journal_path)) or dry_run:
        return None
    try:
        lf = open(os.path.join(gdir, "planter-workdir-build.lock"), "a+", encoding="utf-8")
        os.set_inheritable(lf.fileno(), False)
        fcntl.flock(lf.fileno(), fcntl.LOCK_EX)
        return lf
    except OSError:
        return None


_workdir_build_lock = acquire_workdir_build_lock(git_dir)

if os.path.exists(journal_path):
    try:
        restore_from_journal()
        add(
            "tests_json_parity_restored",
            "INFO",
            "Restored the files of an interrupted tests_json_parity run.",
        )
    except (RestoreConflict, OSError, ValueError, KeyError) as e:
        report_restore_failure(e)
        finish()

if skip_build:
    add(
        "tests_json_parity_skipped",
        "INFO",
        "PLANTER_SKIP_BUILD is set; tests.json parity was not checked.",
    )
    finish()

fx = next(
    (
        os.path.join(workdir, rel)
        for rel in (".jiri_root/bin/fx", "scripts/fx")
        if os.path.isfile(os.path.join(workdir, rel))
    ),
    None,
)
build_dir = (read(".fx-build-dir") or "").strip()
args_gn_rel = (
    os.path.relpath(os.path.join(workdir, build_dir, "args.gn"), workdir)
    if build_dir
    else ""
)
if (
    not fx
    or not build_dir
    or args_gn_rel.startswith("..")
    or read(args_gn_rel) is None
):
    add(
        "tests_json_parity_skipped",
        "INFO",
        "No fx or no configured build dir (`fx set`) in the workdir; "
        "tests.json parity was not checked.",
    )
    finish()
args_gn = os.path.join(workdir, args_gn_rel)
tests_json = os.path.join(os.path.dirname(args_gn), "tests.json")


def in_project_dir(rel):
    real = os.path.realpath(os.path.join(workdir, rel))
    return real == project_dir or real.startswith(project_dir + os.sep)


def in_target_dir(rel):
    return any(rel.startswith(d + "/") for d in target_dirs)


def base_entries(paths_list):
    """Maps each path that exists at $PLANTER_CHANGE_BASE to (mode, blob sha)."""
    entries = {}
    for i in range(0, len(paths_list), 500):
        out = git(
            ["ls-tree", "-z", change_base, "--"] + paths_list[i : i + 500]
        )
        for rec in out.split("\0"):
            if rec:
                meta, rel = rec.split("\t", 1)
                mode, _, blob = meta.split()
                entries[rel] = (mode, blob)
    return entries


try:
    base_commit = git(
        ["rev-parse", "--verify", change_base + "^{commit}"]
    ).strip()
    tracked = git(
        ["diff", "--name-only", "-z", "--no-renames", change_base, "--"]
    ).split("\0")
    untracked = git(
        ["ls-files", "--others", "--exclude-standard", "-z"]
    ).split("\0")
    paths = sorted(
        {p for p in tracked if p}
        | {
            p
            for p in untracked
            if p and (BUILD_FILE.search(p) or in_target_dir(p))
        }
    )
    paths = [p for p in paths if not in_project_dir(p)]
    base = base_entries(paths)
except (OSError, subprocess.CalledProcessError, ValueError) as e:
    add(
        "tests_json_parity_fx_gen_failed",
        "ERROR",
        f"Cannot list the change since {change_base} ({e}); tests.json parity "
        "could not be verified.",
        remediation=FX_GEN_REMEDIATION,
    )
    finish()

# Submodules and nested repositories are not files of this repository.
paths = [
    p
    for p in paths
    if base.get(p, ("",))[0] != "160000"
    and not os.path.isdir(os.path.join(workdir, p, ".git"))
]
if not paths:
    finish()

touched_dirs = set(target_dirs)
for p in paths:
    if os.path.basename(p) in ("BUILD.gn", "BUILD.bazel") and os.path.dirname(p):
        touched_dirs.add(os.path.dirname(p))

base_gn_by_dir = {d: base_text(f"{d}/BUILD.gn") for d in sorted(touched_dirs)}
base_gn_targets = []
for d, gn_txt in sorted(base_gn_by_dir.items()):
    base_gn_targets.extend(vtp.extract_gn_test_targets(gn_txt, d))


def path_conflict(rel):
    """True when reverting `rel` would replace a directory or traverse a symlink."""
    full = os.path.join(workdir, rel)
    if os.path.isdir(full) and not os.path.islink(full):
        return True
    d = os.path.dirname(rel)
    while d:
        a = os.path.join(workdir, d)
        if os.path.islink(a) or (os.path.lexists(a) and not os.path.isdir(a)):
            return True
        d = os.path.dirname(d)
    return False


def run_gen():
    """Runs `fx gen`; returns None on success or the tail of its output."""
    try:
        proc = subprocess.run(
            [fx, "gen"],
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return f"timed out after {timeout}s"
    if proc.returncode == 0:
        return None
    return f"exit {proc.returncode}:\n" + "\n".join(
        proc.stdout.splitlines()[-40:]
    )


def run_gn_refs(lbl):
    """Runs `fx gn refs <build_dir> <lbl>` to find enclosing GN groups."""
    try:
        proc = subprocess.run(
            [fx, "gn", "refs", os.path.join(workdir, build_dir), lbl],
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if proc.returncode != 0:
        return []
    return [
        line.strip()
        for line in proc.stdout.splitlines()
        if line.strip().startswith("//")
    ]


def merge_tests(primary_list, secondary_list):
    """Merges normalized test records from primary product and scoped passes."""
    merged = {}
    for rec in list(primary_list) + list(secondary_list):
        key = (rec["os"], rec["cpu"], rec["name"], rec["variant"])
        merged[key] = rec
    return sorted(
        merged.values(),
        key=lambda r: (r["os"], r["cpu"], r["canonical_id"], r["variant"]),
    )


original_args = read(args_gn_rel)
product_args = vtp.build_product_args_gn(original_args)

commit_msg = ""
msg_file = os.environ.get("PLANTER_COMMIT_MSG_FILE", "").strip()
if msg_file and os.path.isfile(msg_file):
    try:
        with open(msg_file, encoding="utf-8", errors="replace") as fh:
            commit_msg = fh.read()
    except OSError:
        commit_msg = ""
else:
    try:
        head_commit = git(["rev-parse", "--verify", "HEAD^{commit}"]).strip()
        if head_commit != base_commit:
            commit_msg = git(["log", "-1", "--format=%B", "HEAD"])
    except (OSError, subprocess.CalledProcessError):
        commit_msg = ""

jiri_digest = file_digest(os.path.join(workdir, ".jiri_root", "update_history", "latest"))
key_material = json.dumps(
    [
        BASELINE_VERSION,
        base_commit,
        os.path.realpath(args_gn),
        sha(product_args.encode()),
        sha(json.dumps(sorted(touched_dirs)).encode()),
        jiri_digest,
    ]
)
baseline_path = os.path.join(
    state_dir, "baseline-" + sha(key_material.encode())[:24] + ".json"
)
result_cache_path = os.path.join(state_dir, "result-cache.json")
result_key = sha(
    json.dumps(
        [
            key_material,
            sha(original_args.encode()),
            sha(commit_msg.encode()),
            [(p, state(os.path.join(workdir, p))) for p in paths],
        ]
    ).encode()
)
no_reuse = os.environ.get("PLANTER_BUILD_NO_REUSE", "").strip() not in (
    "",
    "0",
    "false",
)
reuse_ttl = int(os.environ.get("PLANTER_BUILD_REUSE_TTL", "21600"))
if not dry_run and not no_reuse:
    try:
        with open(result_cache_path, encoding="utf-8") as f:
            rc_data = json.load(f)
        rc_entry = (
            (rc_data.get("entries") or {}).get(result_key)
            if isinstance(rc_data, dict)
            else None
        )
        if (
            isinstance(rc_entry, dict)
            and 0 <= time.time() - rc_entry.get("time", 0) <= reuse_ttl
        ):
            for rf in rc_entry.get("findings") or []:
                findings.append(rf)
            finish()
    except (OSError, ValueError):
        pass
baseline = None
try:
    with open(baseline_path, encoding="utf-8") as f:
        loaded = json.load(f)
        if isinstance(loaded, dict) and "tests" in loaded and not loaded.get("error"):
            baseline = loaded
except (OSError, ValueError):
    pass

if dry_run:
    plan = []
    if baseline is None:
        plan.append(
            f"write the {change_base} version of {len(paths)} changed files "
            "and run `fx gen` with CQ product roots (plus scoped developer "
            "group roots if any touched GN test targets are off-CQ) for the "
            "baseline"
        )
    plan.append("run `fx gen` against the post-change tree")
    add(
        "build_dry_run",
        "INFO",
        "PLANTER_BUILD_DRY_RUN: tests_json_parity would "
        + ", then ".join(plan)
        + ".",
    )
    finish()


def write_bytes(rel, content, mode="100644"):
    dst = os.path.join(workdir, rel)
    remove_file(dst)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    if mode == "120000":
        os.symlink(content.decode(), dst)
    else:
        with open(dst, "wb") as f:
            f.write(content)
        os.chmod(dst, 0o755 if mode == "100755" else 0o644)


# Report an environment/filesystem write failure (such as ENOENT or EROFS on a
# degraded FUSE/casfs mount) as a structured finding instead of crashing.
def report_workspace_write_error(stage, error):
    msg = (
        f"Filesystem write failed during {stage} ({error}). The checkout or git/build "
        "directory mount is rejecting file writes (for example a degraded FUSE/casfs "
        "mount), so `tests.json` parity and `fx gen` cannot run."
    )
    rem = (
        "This is a workspace filesystem/mount write failure, not a BUILD.bazel or "
        "BUILD.gn defect. Do not edit build files to work around it; stop retrying "
        "failing build or `git commit` write commands, set `test_footers` to "
        '`["fx lint"]` and `disputed_findings` to `[]` in '
        "`CoderReport` (an empty `test_footers` fails `test_verifier` with "
        "`missing_test_footer`, while `fx bazel`/`fx build`/`fx test` target "
        "commands trigger spurious `test_verifier` query errors on an unwritable "
        "mount), and report the mount write error in `summary`."
    )
    add("workspace_write_error", "ERROR", msg, remediation=rem)
    finish()


# Verify that git_dir/state_dir, the build directory, and the checkout accept
# non-empty file writes before backing up or mutating any working-tree files
# (degraded FUSE/casfs mounts allow 0-byte O_CREAT metadata ops while rejecting
# write()). Probe git_dir when state_dir does not exist yet so failed probes do
# not leave empty directories behind in .git.
def verify_workspace_writable():
    probe_bytes = b"planter-write-probe\n"
    git_probe_dir = state_dir if os.path.isdir(state_dir) else git_dir
    probe_dirs = list(
        dict.fromkeys(
            d
            for d in (git_probe_dir, os.path.dirname(args_gn), workdir)
            if os.path.isdir(d)
        )
    )
    for probe_dir in probe_dirs:
        fd = None
        probe_path = None
        try:
            fd, probe_path = tempfile.mkstemp(
                dir=probe_dir, prefix=".planter-write-probe."
            )
            os.write(fd, probe_bytes)
            os.close(fd)
            fd = None
            with open(probe_path, "rb") as rf:
                if rf.read() != probe_bytes:
                    raise OSError("short write during writability probe")
            os.unlink(probe_path)
        except OSError as e:
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
            report_workspace_write_error(f"writability probe in {probe_dir}", e)


def save_baseline(data):
    try:
        os.makedirs(state_dir, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=state_dir, prefix=".baseline.")
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f)
        os.replace(tmp, baseline_path)
        # Each change base and build config adds a baseline; keep the newest few.
        old = sorted(
            (p for p in os.listdir(state_dir) if p.startswith("baseline-")),
            key=lambda p: os.path.getmtime(os.path.join(state_dir, p)),
            reverse=True,
        )
        for p in old[10:]:
            os.unlink(os.path.join(state_dir, p))
    except OSError:
        # Baseline caching is an optimization; ignore cache write failures.
        pass


def with_written_files(writes, action):
    """Writes `writes`, runs `action(update_file_fn)`, and restores originals."""
    entries = []
    entry_by_rel = {}
    for rel, (content, mode) in writes.items():
        if content is None:
            written = "absent"
        elif mode == "120000":
            written = "link:" + content.decode()
        else:
            written = "file:" + sha(content)
        rec = {"path": rel, "written_states": [written]}
        entries.append(rec)
        entry_by_rel[rel] = rec
    try:
        back_up(entries)
    except OSError as e:
        if not os.path.exists(journal_path):
            shutil.rmtree(backup_dir, ignore_errors=True)
        report_workspace_write_error("working-tree backup", e)
    write_err = None

    def rewrite_tracked_file(rel, new_bytes, mode="100644"):
        rec = entry_by_rel[rel]
        new_state = "file:" + sha(new_bytes)
        if new_state not in rec["written_states"]:
            rec["written_states"].append(new_state)
            write_journal(entries)
        write_bytes(rel, new_bytes, mode)

    try:
        for rel, (content, mode) in writes.items():
            if content is None:
                remove_file(os.path.join(workdir, rel))
                prune_empty_dirs(rel)
            else:
                write_bytes(rel, content, mode)
        return action(rewrite_tracked_file)
    except OSError as e:
        write_err = e
        return {"error": str(e)}
    finally:
        try:
            restore_from_journal()
        except (RestoreConflict, OSError, ValueError, KeyError) as e:
            report_restore_failure(e)
            finish()
        if write_err is not None:
            report_workspace_write_error("working-tree write", write_err)


def gen_and_load():
    error = run_gen()
    if error is not None:
        return {"error": error}
    try:
        return {"tests": vtp.load_tests_file(tests_json)}
    except (OSError, ValueError) as e:
        return {"error": f"cannot read <build dir>/tests.json: {e}"}


verify_workspace_writable()
ran_revert_or_custom_args = False

if baseline is None:
    if len(paths) > MAX_REVERTED_PATHS:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            f"The change touches {len(paths)} files since {change_base} (more "
            f"than {MAX_REVERTED_PATHS}); rebase or set PLANTER_CHANGE_BASE "
            "accurately so tests.json parity can be verified.",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()
    conflicts = [p for p in paths if path_conflict(p)]
    if conflicts:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            "The change replaces files with directories or symlinks (or the "
            "reverse) at "
            + ", ".join(conflicts[:5])
            + "; tests.json parity could not revert cleanly.",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()
    try:
        writes = {}
        for rel in paths:
            if rel in base:
                mode, blob = base[rel]
                writes[rel] = (
                    git(["cat-file", "blob", blob], text=False),
                    mode,
                )
            else:
                writes[rel] = (None, "")
    except (OSError, subprocess.CalledProcessError) as e:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            f"Cannot read the {change_base} version of the change ({e}); "
            "tests.json parity could not be verified.",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()
    writes[args_gn_rel] = (product_args.encode(), "100644")

    def _compute_baseline(rewrite_file):
        primary_res = gen_and_load()
        if primary_res.get("error") is not None:
            return primary_res
        merged_tests = primary_res["tests"]
        uncovered = vtp.find_uncovered_gn_test_targets(
            base_gn_targets, merged_tests
        )
        scoped_host_roots = []
        scoped_dev_roots = []
        if uncovered:
            scoped_host_roots, scoped_dev_roots = (
                vtp.resolve_highest_enclosing_groups(
                    uncovered,
                    run_gn_refs,
                    {d: t for d, t in base_gn_by_dir.items() if t},
                    read_gn_fn=lambda rel_dir: read(f"{rel_dir}/BUILD.gn"),
                )
            )
            scoped_args = vtp.build_scoped_args_gn(
                original_args, scoped_host_roots, scoped_dev_roots
            )
            if scoped_args not in (original_args, product_args):
                rewrite_file(args_gn_rel, scoped_args.encode(), "100644")
                scoped_res = gen_and_load()
                if scoped_res.get("error") is not None:
                    return {
                        "error": (
                            "scoped developer-root `fx gen` failed: "
                            + scoped_res["error"]
                        )
                    }
                merged_tests = merge_tests(merged_tests, scoped_res["tests"])
        return {
            "tests": merged_tests,
            "scoped_host_roots": scoped_host_roots,
            "scoped_dev_roots": scoped_dev_roots,
        }

    ran_revert_or_custom_args = True
    baseline = with_written_files(writes, _compute_baseline)
    if baseline.get("error") is None:
        save_baseline(baseline)

after = None
if baseline.get("error") is None:
    scoped_host = baseline.get("scoped_host_roots") or []
    scoped_dev = baseline.get("scoped_dev_roots") or []
    scoped_after_args = (
        vtp.build_scoped_args_gn(original_args, scoped_host, scoped_dev)
        if (scoped_host or scoped_dev)
        else original_args
    )
    need_scoped_after = scoped_after_args not in (original_args, product_args)

    if product_args == original_args and not need_scoped_after:
        after = gen_and_load()
    else:
        ran_revert_or_custom_args = True

        def _compute_after(rewrite_file):
            primary_after = gen_and_load()
            if primary_after.get("error") is not None:
                return primary_after
            merged_after = primary_after["tests"]
            if need_scoped_after:
                rewrite_file(args_gn_rel, scoped_after_args.encode(), "100644")
                scoped_res = gen_and_load()
                if scoped_res.get("error") is not None:
                    return {
                        "error": (
                            "scoped developer-root `fx gen` failed: "
                            + scoped_res["error"]
                        )
                    }
                merged_after = merge_tests(merged_after, scoped_res["tests"])
            return {"tests": merged_after}

        after = with_written_files(
            {args_gn_rel: (product_args.encode(), "100644")}, _compute_after
        )

# Regenerate with original `args.gn` whenever files or `args.gn` were swapped so
# Ninja sees the exact post-change build graph.
if ran_revert_or_custom_args:
    restore_gen_err = run_gen()
    if restore_gen_err is not None:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            f"`fx gen` after restoring the change and args.gn failed ({restore_gen_err}).",
            remediation=FX_GEN_REMEDIATION,
        )
        finish()

if baseline.get("error") is not None:
    add(
        "tests_json_parity_fx_gen_failed",
        "ERROR",
        f"`fx gen` before the change failed ({baseline['error']}); tests.json "
        "parity failed closed.",
        remediation=FX_GEN_REMEDIATION,
    )
    finish()

if after is None or after.get("error") is not None:
    err_detail = after["error"] if after else "unknown error"
    add(
        "tests_json_parity_fx_gen_failed",
        "ERROR",
        f"`fx gen` after the change failed ({err_detail}); tests.json parity "
        "failed closed.",
        remediation=FX_GEN_REMEDIATION,
    )
    finish()

package_states = {}
for d in sorted(touched_dirs):
    package_states[f"//{d}"] = vtp.PackageState(
        pkg_dir=d,
        before_gn=base_gn_by_dir.get(d),
        after_gn=read(f"{d}/BUILD.gn"),
        before_bazel=base_text(f"{d}/BUILD.bazel"),
        after_bazel=read(f"{d}/BUILD.bazel"),
    )

before_tests, after_tests = baseline["tests"], after["tests"]
parity_findings = vtp.verify_parity(
    before_tests=before_tests,
    after_tests=after_tests,
    package_states=package_states,
    commit_msg=commit_msg,
    change_base=change_base,
    workdir=workdir,
)
findings.extend(parity_findings)

if not any(f["severity"] == "ERROR" for f in parity_findings):
    add(
        "tests_json_parity",
        "INFO",
        f"All {len(before_tests)} baseline test entries of "
        f"<build dir>/tests.json before the change satisfy migration parity "
        "after the change.",
    )
    try:
        os.makedirs(state_dir, exist_ok=True)
        rc_entries = {}
        try:
            with open(result_cache_path) as f:
                old_rc = json.load(f)
            if isinstance(old_rc, dict) and isinstance(old_rc.get("entries"), dict):
                rc_entries = old_rc["entries"]
        except (OSError, ValueError):
            pass
        now_ts = time.time()
        rc_entries[result_key] = {"time": now_ts, "findings": list(findings)}
        if len(rc_entries) > 10:
            rc_entries = dict(sorted(rc_entries.items(), key=lambda kv: kv[1].get("time", 0), reverse=True)[:10])
        fd, tmp_rc = tempfile.mkstemp(dir=state_dir, prefix=".result-cache.")
        with os.fdopen(fd, "w") as f:
            json.dump({"entries": rc_entries}, f)
        os.replace(tmp_rc, result_cache_path)
    except OSError:
        pass

finish()
PYEOF
