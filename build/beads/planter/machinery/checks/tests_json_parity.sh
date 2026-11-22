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
BASELINE_VERSION = "6"
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

EMPTY_FILE_STATE = "file:" + sha(b"")


# Report an environment/filesystem write failure (such as ENOENT or EROFS on a
# degraded FUSE/casfs mount) as a structured finding instead of crashing or
# blaming the migration change.
def report_workspace_write_error(stage, error, has_pending_restore=False):
    pending_note = (
        f" Original working-tree files remain backed up in {journal_path} and "
        f"{backup_dir} and will be restored automatically once writes succeed."
        if has_pending_restore
        else ""
    )
    msg = (
        f"Filesystem write failed during {stage} ({error}). The checkout or "
        "git/build directory mount is rejecting file writes (for example a "
        "degraded FUSE/casfs mount), so `tests.json` parity and `fx gen` "
        f"cannot run.{pending_note}"
    )
    rem = (
        "This is a workspace filesystem/mount write failure, not a BUILD.bazel "
        "or BUILD.gn defect. Do not edit build files or delete "
        "`planter-tests-json-parity/restore.json` or `backup` to work around "
        "it; stop retrying failing build or `git commit` write commands, keep "
        "the task commit's changed files (`git diff --name-only HEAD~1 HEAD` "
        "when `HEAD` already has the local migration commit) in "
        "`CoderReport.modified_files` rather than `[]` (so "
        "`PLANTER_CHANGE_BASE=HEAD` retries do not fail with "
        "`empty_migration_diff`), set `test_footers` to `[\"fx lint\"]` and "
        "`disputed_findings` to `[]` in `CoderReport` (an empty `test_footers` "
        "fails `test_verifier` with `missing_test_footer`, while target-bearing "
        "`Test:` commands trigger spurious `test_verifier` query errors on an "
        "unwritable mount), and report the mount write error in `summary`."
    )
    add("workspace_write_error", "ERROR", msg, remediation=rem)
    finish()


# Probe that each existing directory in `candidate_dirs` accepts non-empty file
# writes (degraded FUSE/casfs mounts allow 0-byte O_CREAT metadata ops while
# rejecting write()). Returns `(probe_dir, error)` on failure or `None` when ok.
def probe_workspace_writable(candidate_dirs):
    probe_bytes = b"planter-write-probe\n"
    probe_dirs = list(
        dict.fromkeys(d for d in candidate_dirs if d and os.path.isdir(d))
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
            return probe_dir, e
    return None


# Stage file bytes in a sibling temporary file and atomically replace `dst` so a
# failing write() on a degraded mount never truncates or unlinks `dst` or leaves
# orphaned 0-byte temporary files behind.
def atomic_write_bytes(dst, data, mode=0o644):
    parent = os.path.dirname(dst) or workdir
    os.makedirs(parent, exist_ok=True)
    fd = None
    tmp = None
    try:
        fd, tmp = tempfile.mkstemp(dir=parent, prefix=".planter-tjp.")
        view = memoryview(data)
        total = 0
        while total < len(view):
            written = os.write(fd, view[total:])
            if written <= 0:
                raise OSError("short write while staging file")
            total += written
        os.chmod(fd, mode)
        os.close(fd)
        fd = None
        os.replace(tmp, dst)
        tmp = None
    finally:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass
        if tmp is not None and os.path.lexists(tmp):
            try:
                os.unlink(tmp)
            except OSError:
                pass


# Stage a symlink at a sibling temporary path and atomically replace `dst`.
def atomic_write_symlink(dst, target_str):
    parent = os.path.dirname(dst) or workdir
    os.makedirs(parent, exist_ok=True)
    fd = None
    tmp = None
    try:
        fd, tmp = tempfile.mkstemp(dir=parent, prefix=".planter-tjp-lnk.")
        os.close(fd)
        fd = None
        os.unlink(tmp)
        os.symlink(target_str, tmp)
        os.replace(tmp, dst)
        tmp = None
    finally:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass
        if tmp is not None and os.path.lexists(tmp):
            try:
                os.unlink(tmp)
            except OSError:
                pass


# Copy a regular file or symlink to `dst` via atomic replacement rather than
# in-place O_TRUNC (`shutil.copy2`), preserving `dst` if `write()` fails.
def atomic_copy_entry(src, dst):
    if os.path.islink(src):
        atomic_write_symlink(dst, os.readlink(src))
        return
    st_mode = os.stat(src).st_mode & 0o777
    with open(src, "rb") as sf:
        data = sf.read()
    atomic_write_bytes(dst, data, st_mode or 0o644)


# Serialize concurrent tests_json_parity invocations across background and post-report
# check runs so an overlapping run never restores or deletes an active run's backup_dir.
lock_fd = None
try:
    os.makedirs(state_dir, exist_ok=True)
    lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o644)
    if skip_build:
        fcntl.flock(lock_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    elif not dry_run:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
except BlockingIOError:
    add(
        "tests_json_parity_skipped",
        "INFO",
        "PLANTER_SKIP_BUILD is set; tests.json parity was not checked.",
    )
    finish()
except OSError as e:
    if skip_build:
        add(
            "tests_json_parity_skipped",
            "INFO",
            "PLANTER_SKIP_BUILD is set; tests.json parity was not checked.",
        )
        finish()
    if not dry_run:
        report_workspace_write_error(
            f"lock initialization in {state_dir}",
            e,
            has_pending_restore=os.path.exists(journal_path),
        )


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
    payload = json.dumps({"paths": entries}).encode("utf-8")
    atomic_write_bytes(journal_path, payload, 0o600)


def back_up(entries):
    """Backs up the files of `entries` and writes the journal."""
    os.makedirs(state_dir, exist_ok=True)
    shutil.rmtree(backup_dir, ignore_errors=True)
    for e in entries:
        src = os.path.join(workdir, e["path"])
        e["backup_state"] = state(src)
        if e["backup_state"] != "absent":
            dst = os.path.join(backup_dir, e["path"])
            atomic_copy_entry(src, dst)
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


# Collect existing directories that `restore_from_journal` will write into so
# writability is verified before touching any journaled working-tree file.
def restore_probe_dirs():
    git_probe = state_dir if os.path.isdir(state_dir) else git_dir
    dirs_to_probe = [git_probe, workdir]
    try:
        with open(journal_path, encoding="utf-8") as jf:
            entries = json.load(jf).get("paths") or []
        for e in entries:
            rel = e.get("path") if isinstance(e, dict) else ""
            if not rel:
                continue
            cur_parent = os.path.dirname(os.path.join(workdir, rel))
            while cur_parent and not os.path.isdir(cur_parent):
                nxt = os.path.dirname(cur_parent)
                if nxt == cur_parent:
                    break
                cur_parent = nxt
            if cur_parent and os.path.isdir(cur_parent):
                dirs_to_probe.append(cur_parent)
    except (OSError, ValueError, AttributeError):
        pass
    return dirs_to_probe


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
        # Also treat 0-byte files left by a prior interrupted O_TRUNC copy/write
        # as un-restored when the backed-up original was non-empty.
        lost_during_restore = backup_st.startswith("file:") and (
            (current == "absent" and "absent" not in written_sts)
            or (current == EMPTY_FILE_STATE and backup_st != EMPTY_FILE_STATE)
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
            bkp_src = os.path.join(backup_dir, rel)
            if os.path.lexists(bkp_src) and (
                not backup_st or state(bkp_src) == backup_st
            ):
                atomic_copy_entry(bkp_src, dst)
            else:
                recovered = recover_missing_backup_bytes(e)
                if recovered is not None:
                    atomic_write_bytes(dst, recovered, 0o644)
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

if os.path.exists(journal_path) and not dry_run:
    probe_err = probe_workspace_writable(restore_probe_dirs())
    if probe_err is not None:
        if not skip_build:
            report_workspace_write_error(
                f"writability probe in {probe_err[0]}",
                probe_err[1],
                has_pending_restore=True,
            )
    else:
        try:
            restore_from_journal()
            add(
                "tests_json_parity_restored",
                "INFO",
                "Restored the files of an interrupted tests_json_parity run.",
            )
        except OSError as e:
            if not skip_build:
                report_workspace_write_error(
                    "working-tree restore",
                    e,
                    has_pending_restore=os.path.exists(journal_path),
                )
        except (RestoreConflict, ValueError, KeyError) as e:
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


# Terminate an `fx gen` process group cleanly on timeout or interrupt so child
# `gn` / `regenerator.py` processes do not hold pipes open or block tree restore.
def _terminate_gen_process_group(proc):
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(proc.pid, sig)
        except OSError:
            pass
        try:
            proc.wait(timeout=2 if sig == signal.SIGTERM else 5)
            break
        except subprocess.TimeoutExpired:
            continue
    if proc.stdout is not None:
        try:
            proc.stdout.close()
        except OSError:
            pass
    try:
        proc.wait(timeout=1)
    except subprocess.TimeoutExpired:
        pass


def run_gen():
    """Runs `fx gen` in its own process group; returns None on success or error tail."""
    try:
        proc = subprocess.Popen(
            [fx, "gen"],
            cwd=workdir,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            start_new_session=True,
        )
    except OSError as e:
        return f"failed to start fx gen: {e}"
    try:
        out, _ = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        _terminate_gen_process_group(proc)
        return f"timed out after {timeout}s"
    except BaseException:
        _terminate_gen_process_group(proc)
        raise
    if proc.returncode == 0:
        return None
    return f"exit {proc.returncode}:\n" + "\n".join(out.splitlines()[-40:])


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
after_gn_by_dir = {d: read(f"{d}/BUILD.gn") for d in sorted(touched_dirs)}
base_sanitized_args = vtp.sanitize_args_gn(
    original_args, gn_by_dir=base_gn_by_dir
)
product_args = vtp.build_product_args_gn(
    original_args, gn_by_dir=base_gn_by_dir
)
after_product_args = vtp.build_product_args_gn(
    original_args, gn_by_dir=after_gn_by_dir
)

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
# Key baseline cache on base commit + product args + jiri state so ancillary
# touched_dirs edits without uncovered off-CQ GN test targets reuse the baseline
# instead of re-reverting the live working tree.
base_key = sha(
    json.dumps(
        [
            BASELINE_VERSION,
            base_commit,
            os.path.realpath(args_gn),
            sha(product_args.encode()),
            jiri_digest,
        ]
    ).encode()
)
key_material = json.dumps(
    [
        base_key,
        sha(json.dumps(sorted(touched_dirs)).encode()),
    ]
)
baseline_path = os.path.join(state_dir, "baseline-" + base_key[:24] + ".json")
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
# Check whether a cached baseline already covers every GN test target in `gn_targets`.
def _baseline_covers_gn_targets(loaded_data, gn_targets):
    if (
        not isinstance(loaded_data, dict)
        or "tests" not in loaded_data
        or loaded_data.get("error")
    ):
        return False
    covered = set(loaded_data.get("covered_gn_labels") or ())
    unchecked = [t for t in gn_targets if t["label"] not in covered]
    if not unchecked:
        return True
    return not vtp.find_uncovered_gn_test_targets(unchecked, loaded_data["tests"])


# Load a valid cached baseline for `base_key` without reverting working-tree files.
def _load_reusable_baseline():
    cand_paths = [baseline_path]
    if os.path.isdir(state_dir):
        try:
            for name in sorted(os.listdir(state_dir)):
                if name.startswith("baseline-") and name.endswith(".json"):
                    full = os.path.join(state_dir, name)
                    if full != baseline_path:
                        cand_paths.append(full)
        except OSError:
            pass
    for cpath in cand_paths:
        try:
            with open(cpath, encoding="utf-8") as f:
                loaded = json.load(f)
        except (OSError, ValueError):
            continue
        if not isinstance(loaded, dict):
            continue
        if cpath != baseline_path and loaded.get("base_key") != base_key:
            continue
        if loaded.get("base_key") not in (None, base_key):
            continue
        if _baseline_covers_gn_targets(loaded, base_gn_targets):
            return loaded
    return None


baseline = _load_reusable_baseline()

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
    if mode == "120000":
        atomic_write_symlink(dst, content.decode())
    else:
        atomic_write_bytes(dst, content, 0o755 if mode == "100755" else 0o644)


# Verify that git_dir/state_dir, the build directory, and the checkout accept
# non-empty file writes before backing up or mutating any working-tree files.
# Probe git_dir when state_dir does not exist yet so failed probes do not leave
# empty directories behind in .git.
def verify_workspace_writable():
    git_probe_dir = state_dir if os.path.isdir(state_dir) else git_dir
    probe_err = probe_workspace_writable(
        (git_probe_dir, os.path.dirname(args_gn), workdir)
    )
    if probe_err is not None:
        report_workspace_write_error(
            f"writability probe in {probe_err[0]}", probe_err[1]
        )


def save_baseline(data):
    try:
        os.makedirs(state_dir, exist_ok=True)
        atomic_write_bytes(
            baseline_path, json.dumps(data).encode("utf-8"), 0o600
        )
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
        # Stage all non-empty writes before unlinking newly added files so a
        # failing write() aborts before removing any working-tree file.
        for rel, (content, mode) in writes.items():
            if content is not None:
                write_bytes(rel, content, mode)
        for rel, (content, _mode) in writes.items():
            if content is None:
                remove_file(os.path.join(workdir, rel))
                prune_empty_dirs(rel)
        return action(rewrite_tracked_file)
    except OSError as e:
        write_err = e
        return {"error": str(e)}
    finally:
        try:
            restore_from_journal()
        except OSError as e:
            report_workspace_write_error(
                "working-tree restore",
                e,
                has_pending_restore=os.path.exists(journal_path),
            )
        except (RestoreConflict, ValueError, KeyError) as e:
            report_restore_failure(e)
            finish()
        if write_err is not None:
            report_workspace_write_error(
                "working-tree write",
                write_err,
                has_pending_restore=os.path.exists(journal_path),
            )


def gen_and_load():
    error = run_gen()
    if error is not None:
        return {"error": error}
    try:
        return {"tests": vtp.load_tests_file(tests_json)}
    except (OSError, ValueError) as e:
        return {"error": f"cannot read <build dir>/tests.json: {e}"}


# Check whether a GN group root label still has its BUILD.gn file and target
# on disk (full BUILD.gn removal or wrapper-only BUILD.gn retention deletes
# an off-CQ leaf package's `group("tests")`).
def _gn_root_pkg_has_build_gn(lbl):
    if not lbl.startswith("//"):
        return True
    rel_pkg, sep, raw_tgt = lbl[2:].partition(":")
    rel_pkg = rel_pkg.strip("/")
    tgt_name = (
        raw_tgt.split("(", 1)[0].strip()
        if sep
        else os.path.basename(rel_pkg)
    )
    gn_p = (
        os.path.join(workdir, rel_pkg, "BUILD.gn")
        if rel_pkg
        else os.path.join(workdir, "BUILD.gn")
    )
    if (
        not os.path.isfile(gn_p)
        and rel_pkg
        and not rel_pkg.startswith("build/secondary/")
    ):
        gn_p = os.path.join(workdir, "build", "secondary", rel_pkg, "BUILD.gn")
    if not os.path.isfile(gn_p):
        return False
    if rel_pkg not in touched_dirs or not tgt_name:
        return True
    try:
        with open(gn_p, encoding="utf-8", errors="replace") as fh:
            gn_clean = vtp._blank_gn_comments(fh.read())
    except OSError:
        return False
    if re.search(
        r'\b[A-Za-z_]\w*\(\s*"' + re.escape(tgt_name) + r'"\s*\)', gn_clean
    ):
        return True
    if re.search(r"\bstandardized_test_groups\s*\(", gn_clean):
        return True
    for suffix, tmpl_re in (
        ("_test", r"rustc_library|rustc_binary|rustc_proc_macro"),
        ("_tests", r"ffx_plugin"),
    ):
        if tgt_name.endswith(suffix) and len(tgt_name) > len(suffix):
            owner = tgt_name[: -len(suffix)]
            if re.search(
                r"\b(?:" + tmpl_re + r')\(\s*"' + re.escape(owner) + r'"\s*\)',
                gn_clean,
            ):
                return True
    return False


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
            def _read_gn_dir(rel_dir):
                txt = read(f"{rel_dir}/BUILD.gn" if rel_dir else "BUILD.gn")
                if (
                    txt is not None
                    or not rel_dir
                    or rel_dir.startswith("build/secondary/")
                ):
                    return txt
                return read(f"build/secondary/{rel_dir}/BUILD.gn")

            scoped_host_roots, scoped_dev_roots = (
                vtp.resolve_highest_enclosing_groups(
                    uncovered,
                    run_gn_refs,
                    {d: t for d, t in base_gn_by_dir.items() if t},
                    read_gn_fn=_read_gn_dir,
                )
            )
            # Drop roots already covered by the primary CQ product pass (`//bundles/buildbot/core`
            # and `//bundles/buildbot/host`, including normalized `:core` / `:host` labels).
            primary_covered_roots = {
                vtp.PRODUCT_TARGET_ROOT,
                vtp.normalize_label(vtp.PRODUCT_TARGET_ROOT),
                vtp.PRODUCT_HOST_ROOT,
                vtp.normalize_label(vtp.PRODUCT_HOST_ROOT),
            }
            scoped_host_roots = [
                r for r in scoped_host_roots if r not in primary_covered_roots
            ]
            scoped_dev_roots = [
                r for r in scoped_dev_roots if r not in primary_covered_roots
            ]
            scoped_args = vtp.build_scoped_args_gn(
                original_args,
                scoped_host_roots,
                scoped_dev_roots,
                gn_by_dir=base_gn_by_dir,
            )
            if (scoped_host_roots or scoped_dev_roots) and (
                scoped_args != product_args
            ):
                rewrite_file(args_gn_rel, scoped_args.encode(), "100644")
                scoped_res = gen_and_load()
                if scoped_res.get("error") is not None:
                    # Off-CQ leaf groups disconnected from CQ product roots may have
                    # pre-existing dangling BUILD.gn deps at $PLANTER_CHANGE_BASE
                    # (e.g. after an upstream Case 1 BUILD.gn removal); keep only
                    # off-CQ roots that actually generate cleanly at baseline.
                    total_scoped = len(scoped_host_roots) + len(scoped_dev_roots)
                    valid_host = []
                    valid_dev = []
                    if 1 < total_scoped <= 8:
                        for hr in scoped_host_roots:
                            cand_a = vtp.build_scoped_args_gn(
                                original_args, [hr], [], gn_by_dir=base_gn_by_dir
                            )
                            rewrite_file(args_gn_rel, cand_a.encode(), "100644")
                            r_res = gen_and_load()
                            if r_res.get("error") is None:
                                valid_host.append(hr)
                                merged_tests = merge_tests(
                                    merged_tests, r_res["tests"]
                                )
                        for dr in scoped_dev_roots:
                            cand_a = vtp.build_scoped_args_gn(
                                original_args, [], [dr], gn_by_dir=base_gn_by_dir
                            )
                            rewrite_file(args_gn_rel, cand_a.encode(), "100644")
                            r_res = gen_and_load()
                            if r_res.get("error") is None:
                                valid_dev.append(dr)
                                merged_tests = merge_tests(
                                    merged_tests, r_res["tests"]
                                )
                    elif total_scoped > 8:
                        return {
                            "error": (
                                "scoped developer-root `fx gen` failed: "
                                + scoped_res["error"]
                            )
                        }
                    scoped_host_roots = valid_host
                    scoped_dev_roots = valid_dev
                else:
                    merged_tests = merge_tests(merged_tests, scoped_res["tests"])
        return {
            "tests": merged_tests,
            "scoped_host_roots": scoped_host_roots,
            "scoped_dev_roots": scoped_dev_roots,
        }

    ran_revert_or_custom_args = True
    baseline = with_written_files(writes, _compute_baseline)
    if baseline.get("error") is None:
        baseline["base_key"] = base_key
        baseline["covered_gn_labels"] = sorted(
            {t["label"] for t in base_gn_targets}
            | set(baseline.get("covered_gn_labels") or ())
        )
        save_baseline(baseline)

after = None
if baseline.get("error") is None:
    scoped_host = [
        r
        for r in (baseline.get("scoped_host_roots") or [])
        if _gn_root_pkg_has_build_gn(r)
    ]
    scoped_dev = [
        r
        for r in (baseline.get("scoped_dev_roots") or [])
        if _gn_root_pkg_has_build_gn(r)
    ]
    scoped_after_args = (
        vtp.build_scoped_args_gn(
            original_args, scoped_host, scoped_dev, gn_by_dir=after_gn_by_dir
        )
        if (scoped_host or scoped_dev)
        else after_product_args
    )
    need_scoped_after = bool(scoped_host or scoped_dev) and (
        scoped_after_args != after_product_args
    )

    if after_product_args == original_args and not need_scoped_after:
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
            {args_gn_rel: (after_product_args.encode(), "100644")},
            _compute_after,
        )

# Check whether a post-restore `fx gen` failure is caused solely by pre-refresh
# `# planter-managed` test labels in `args.gn` (GN `Unresolved dependencies`
# diagnostics do not include `# planter-managed` comments from `args.gn`).
def is_stale_planter_managed_restore_failure(gen_err):
    if after is None or after.get("error") is not None:
        return False
    stripped_args = vtp.strip_planter_managed_args_gn(original_args)
    if stripped_args == original_args:
        return False
    if "planter-managed" in gen_err:
        return True
    stripped_gen_err = with_written_files(
        {args_gn_rel: (stripped_args.encode(), "100644")},
        lambda _rewrite: run_gen(),
    )
    return stripped_gen_err is None


# Regenerate with original `args.gn` whenever files or `args.gn` were swapped so
# Ninja sees the exact post-change build graph, then report any baseline or
# post-change product-root `fx gen` failure before inspecting `restore_gen_err`
# so pre-refresh `# planter-managed` labels in `args.gn` never mask real build
# graph errors.
restore_gen_err = run_gen() if ran_revert_or_custom_args else None

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

if restore_gen_err is not None:
    if is_stale_planter_managed_restore_failure(restore_gen_err):
        add(
            "tests_json_parity_planter_args_stale",
            "INFO",
            "`fx gen` succeeded with CQ product roots; restoring pre-refresh "
            f"`# planter-managed` `args.gn` labels reported: {restore_gen_err}",
        )
    else:
        add(
            "tests_json_parity_fx_gen_failed",
            "ERROR",
            f"`fx gen` after restoring the change and args.gn failed "
            f"({restore_gen_err}).",
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
