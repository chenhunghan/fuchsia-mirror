#!/usr/bin/env bash
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

set -uo pipefail

export GIT_TERMINAL_PROMPT="${GIT_TERMINAL_PROMPT:-0}"

# Runs the deterministic checks of checks/manifest.json the way planter runs them
# after the coder reports: same scripts, arguments, environment, order, JSON
# parsing, severity normalization and de-duplication.
#
# Usage: run_checks.sh [--skip-build] [--only NAME[,NAME...]] [--json] [--list] [DIR...]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${PLANTER_PROJECT_DIR:-$(dirname "$SCRIPT_DIR")}"

python3 - "$PROJECT_DIR" "$@" <<'PYEOF'
import atexit
from concurrent.futures import ThreadPoolExecutor
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time

USAGE = """\
usage: run_checks.sh [--skip-build] [--only NAME[,NAME...]] [--json] [--list] [DIR...]

Runs the checks in checks/manifest.json the way planter runs them after the
coder reports. Exits 1 if any ERROR or WARNING finding would send the change
back, 0 otherwise.

  --skip-build  set PLANTER_SKIP_BUILD=1 so build_verification does not build
  --only NAMES  run only these checks (comma-separated, repeatable)
  --json        print all findings as one JSON array instead of a report
  --list        list the checks and exit
  DIR...        target directories (default: $PLANTER_TARGET_DIRS)"""

SERIAL_BUILD_CHECKS = ("tests_json_parity", "build_verification", "cutover_consistency")
# Must match planter's upload gate (mergeReviewDiffFindings): only visibility_narrowed
# honors check waivers. Listing other review_diff categories here hides blocking
# diffengine findings during coder runs that planter's upload gate still rejects.
WAIVABLE_REVIEW_DIFF_CATEGORIES = {"visibility_narrowed"}

sys.stdout.reconfigure(errors="replace")
sys.stderr.reconfigure(errors="replace")


def log(msg):
    print(f"[run_checks] {msg}", file=sys.stderr, flush=True)


def die(msg, show_usage=False):
    suffix = f"\n\n{USAGE}" if show_usage else ""
    print(f"run_checks: {msg}{suffix}", file=sys.stderr)
    sys.exit(2)


project_dir = os.path.abspath(sys.argv[1])
alt_root = os.path.join(project_dir, "machinery")
mroot = (
    alt_root
    if os.path.isdir(alt_root)
    and not os.path.islink(alt_root)
    and os.path.exists(os.path.join(alt_root, "prompts", "coder.md"))
    and not os.path.exists(os.path.join(project_dir, "prompts", "coder.md"))
    else project_dir
)

sys.path.insert(0, os.path.join(mroot, "checks"))
import planter_check_lib as pcl
import rdeps_lib as rl

skip_build = as_json = list_only = False
only, dir_args, args, i = [], [], sys.argv[2:], 0
while i < len(args):
    a = args[i]
    if a in ("-h", "--help"):
        print(USAGE)
        sys.exit(0)
    elif a == "--skip-build":
        skip_build = True
    elif a == "--json":
        as_json = True
    elif a == "--list":
        list_only = True
    elif a == "--only" or a.startswith("--only="):
        if a == "--only":
            i += 1
            if i >= len(args):
                die("--only needs a comma-separated list of check names", True)
            value = args[i]
        else:
            value = a[len("--only="):]
        names = [n.strip() for n in value.split(",") if n.strip()]
        if not names:
            die("--only needs a comma-separated list of check names", True)
        only.extend(names)
    elif a == "--":
        dir_args.extend(args[i + 1:])
        break
    elif a.startswith("-"):
        die(f"unknown option {a}", True)
    else:
        dir_args.append(a)
    i += 1

manifest_path = os.path.join(mroot, "checks", "manifest.json")
try:
    specs = pcl.load_check_manifest(manifest_path)
except ValueError as e:
    die(f"cannot parse {manifest_path} (planter would fail the same way): {e}")
if specs is None:
    log(f"no readable {manifest_path}; planter runs no checks")
    specs = []

if list_only:
    for s in specs:
        print(f"{s['name']}{'' if s['enabled'] else ' [disabled]'}: {s['description']}")
    sys.exit(0)

enabled = [s for s in specs if s["enabled"]]
disabled = [s for s in specs if not s["enabled"]]
if only:
    known = {s["name"] for s in specs} | {"review_diff"}
    unknown = [n for n in only if n not in known]
    if unknown:
        die(f"unknown check(s): {', '.join(unknown)}; checks: {', '.join(s['name'] for s in specs)}, review_diff", True)
    wanted = set(only)
    enabled = [s for s in enabled if s["name"] in wanted]
    disabled = [s for s in disabled if s["name"] in wanted]

# Target directory arguments may be whitespace- or comma-delimited; order is preserved.
workdir = os.environ.get("PLANTER_WORKDIR", "") or os.getcwd()
if dir_args:
    target_dirs = pcl.parse_target_dirs(" ".join(dir_args), workdir)
else:
    target_dirs = pcl.parse_target_dirs(os.environ.get("PLANTER_TARGET_DIRS", ""), workdir)
    if not target_dirs and os.environ.get("PLANTER_TARGET_DIR", "").strip():
        target_dirs = pcl.parse_target_dirs(os.environ["PLANTER_TARGET_DIR"], workdir)

# Fetch origin/main at most once up front, then export PLANTER_SKIP_FETCH=1 to all checks.
allow_origin_fetch = os.environ.get("PLANTER_SKIP_FETCH", "").strip() in ("", "0")
if allow_origin_fetch:
    for repo in rl.discover_git_repos(workdir):
        rl.maybe_refresh_repo_origin_main(repo)
os.environ["PLANTER_SKIP_FETCH"] = "1"


# Refresh `prebuilt/tools/shac/shac` from the CIPD version pinned in `manifests/jiri.lock`
# or `manifests/prebuilts` when `shac.textproto` requires a newer `min_shac_version` (including
# during `--skip-build` or `--only migration_sanity` preflight runs).
def _refresh_stale_shac_prebuilt(wdir):
    if not wdir or not os.path.isdir(wdir):
        return
    try:
        with open(os.path.join(wdir, "shac.textproto"), encoding="utf-8", errors="replace") as f:
            shac_cfg = f.read()
    except OSError:
        return
    m_req = re.search(r'(?m)^\s*min_shac_version\s*:\s*"(\d+)\.(\d+)\.(\d+)"', shac_cfg)
    if not m_req:
        return
    req_tuple = (int(m_req.group(1)), int(m_req.group(2)), int(m_req.group(3)))
    shac_dir = os.path.join(wdir, "prebuilt", "tools", "shac")
    shac_bin = os.path.join(shac_dir, "shac")
    if os.path.isfile(shac_bin):
        try:
            ver_out = subprocess.check_output(
                [shac_bin, "version"],
                cwd=wdir,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=10,
            )
            m_cur = re.search(r"(\d+)\.(\d+)\.(\d+)", ver_out or "")
            if m_cur and (int(m_cur.group(1)), int(m_cur.group(2)), int(m_cur.group(3))) >= req_tuple:
                return
        except Exception:
            pass
    arch = "arm64" if os.uname().machine in ("aarch64", "arm64") else "amd64"
    cipd_pkg = f"fuchsia/shac/linux-{arch}"
    pinned_ver = ""
    try:
        with open(os.path.join(wdir, "manifests", "jiri.lock"), encoding="utf-8", errors="replace") as f:
            for entry in json.load(f):
                if isinstance(entry, dict) and entry.get("package") == cipd_pkg:
                    pinned_ver = str(entry.get("version", "")).strip()
                    if pinned_ver:
                        break
    except Exception:
        pass
    if not pinned_ver:
        try:
            with open(os.path.join(wdir, "manifests", "prebuilts"), encoding="utf-8", errors="replace") as f:
                m_pb = re.search(
                    r'name="fuchsia/shac/\$\{platform\}"[\s\S]{0,300}?version="([^"]+)"',
                    f.read(),
                )
                if m_pb:
                    pinned_ver = m_pb.group(1).strip()
        except OSError:
            pass
    cipd_bin = os.path.join(wdir, ".jiri_root", "bin", "cipd")
    if not os.path.isfile(cipd_bin):
        cipd_bin = shutil.which("cipd") or ""
    if not pinned_ver or not cipd_bin or not os.path.isdir(shac_dir):
        return
    tmp_dir = None
    tmp_bin = None
    try:
        tmp_dir = tempfile.mkdtemp(prefix="planter-cipd-shac-")
        subprocess.run(
            [cipd_bin, "export", "-root", tmp_dir, "-ensure-file", "-"],
            input=f"{cipd_pkg} {pinned_ver}\n",
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=60,
            check=True,
        )
        exported_shac = os.path.join(tmp_dir, "shac")
        if not os.path.isfile(exported_shac):
            return
        fd, tmp_bin = tempfile.mkstemp(dir=shac_dir, prefix=".shac-prebuilt.")
        os.close(fd)
        shutil.copyfile(exported_shac, tmp_bin)
        os.chmod(tmp_bin, 0o755)
        os.replace(tmp_bin, shac_bin)
        tmp_bin = None
    except Exception:
        pass
    finally:
        if tmp_bin and os.path.lexists(tmp_bin):
            try:
                os.unlink(tmp_bin)
            except OSError:
                pass
        if tmp_dir and os.path.isdir(tmp_dir):
            shutil.rmtree(tmp_dir, ignore_errors=True)


_refresh_stale_shac_prebuilt(workdir)

# Preserve the caller's raw PLANTER_CHANGE_BASE before resolving PLANTER_RESOLVED_CHANGE_BASE
# so child checks can detect premature pre-upload Git commits when PLANTER_CHANGE_BASE=HEAD.
os.environ.setdefault("PLANTER_ORIG_CHANGE_BASE", os.environ.get("PLANTER_CHANGE_BASE", ""))
os.environ["PLANTER_IN_RUN_CHECKS"] = "1"

# Resolve PLANTER_RESOLVED_CHANGE_BASE and PLANTER_CHANGED_FILES once up front.
change_base = pcl.resolve_change_base(workdir, target_dirs=target_dirs)
resolved_changed_files = pcl.changed_files(
    workdir, change_base, target_dirs=target_dirs
)
os.environ["PLANTER_RESOLVED_CHANGE_BASE"] = change_base
os.environ["PLANTER_CHANGED_FILES"] = "\n".join(resolved_changed_files)

# Map upstream-relocated target directories to their relocated path when the active
# change already modifies the relocated package (for example after rebasing a CL).
if target_dirs and resolved_changed_files:
    remapped_dirs = []
    for td in target_dirs:
        reloc = pcl.resolve_relocated_target_dir(workdir, td)
        mapped = (
            reloc
            if reloc
            and any(
                f == reloc or f.startswith(reloc + "/")
                for f in resolved_changed_files
            )
            else td
        )
        if mapped not in remapped_dirs:
            remapped_dirs.append(mapped)
    target_dirs = remapped_dirs

signal.signal(signal.SIGTERM, lambda signum, _frame: sys.exit(128 + signum))


def execute(cmd, cwd, env, label):
    return pcl.execute_subprocess(cmd, cwd, env, label=label, log_fn=log)


_precomputed_diff_findings = None


def precompute_review_diff():
    global _precomputed_diff_findings
    planter_bin = rl.find_planter_bin()
    if not planter_bin or not workdir:
        return
    env = dict(os.environ, PLANTER_BIN=planter_bin, PLANTER_WORKDIR=workdir,
               PLANTER_TARGET_DIRS=" ".join(target_dirs), PLANTER_CHANGE_BASE=change_base,
               PLANTER_RESOLVED_CHANGE_BASE=change_base, PLANTER_SKIP_FETCH="1",
               PLANTER_SKIP_DIFF_COVERAGE="1")
    for flag in ("--format=findings-json", "--check-findings"):
        out, _, err_text = execute([planter_bin, "diff", "--local", flag, f"--workdir={workdir}"], workdir, env, "review_diff")
        findings, parsed_ok = pcl.parse_check_findings(out.strip())
        if parsed_ok or (err_text is None and out.strip().startswith("[")):
            _precomputed_diff_findings = findings
            break
    if _precomputed_diff_findings is None:
        _precomputed_diff_findings = []
    try:
        tf = tempfile.NamedTemporaryFile("w", prefix="planter-review-diff-", suffix=".json", delete=False)
        json.dump(_precomputed_diff_findings, tf)
        tf.close()
        os.environ["PLANTER_REVIEW_DIFF_JSON"] = tf.name
        atexit.register(lambda p=tf.name: os.path.exists(p) and os.unlink(p))
    except OSError:
        pass


def run_once(spec, target_dir):
    script = spec["script_file"]
    if not os.path.isabs(script):
        for cand in (
            os.path.normpath(os.path.join(mroot, "checks", script)),
            os.path.normpath(os.path.join(project_dir, "checks", script)),
            os.path.normpath(os.path.join(mroot, script)),
            os.path.normpath(os.path.join(project_dir, script)),
        ):
            if os.path.exists(cand):
                script = cand
                break
        else:
            return [pcl.new_finding(
                source=spec["name"], category="check_script_missing_in_pin", severity="INFO",
                message=f"check script {json.dumps(spec['script_file'])} is not present in this pinned machinery snapshot; skipping.",
            )]
    path = os.environ.get("PATH", "")
    env = dict(os.environ, SMELT_PROJECT_DIR=project_dir, PLANTER_PROJECT_DIR=project_dir,
               PLANTER_WORKDIR=workdir, PLANTER_CHECK_NAME=spec["name"],
               PLANTER_TARGET_DIR=target_dir, PLANTER_TARGET_DIRS=" ".join(target_dirs),
               PLANTER_CHANGE_BASE=change_base, PLANTER_RESOLVED_CHANGE_BASE=change_base,
               PLANTER_SKIP_FETCH="1",
               PATH=os.path.join(project_dir, "tools") + (os.pathsep + path if path else ""))
    if skip_build:
        env["PLANTER_SKIP_BUILD"] = "1"
    label = spec["name"] + (f" ({target_dir})" if len(target_dirs) > 1 else "")
    out, err, err_text = execute(["bash", script] + spec["args"], workdir or project_dir, env, label)
    raw = out.strip()
    findings, parsed_ok = pcl.parse_check_findings(raw)
    for f in findings:
        f["source"] = f["source"] or spec["name"]
        f["severity"] = pcl.normalize_severity(f["severity"])
    if err_text is not None and (not parsed_ok or not any(pcl.is_blocking_finding(f) for f in findings)):
        findings.append(pcl.new_finding(
            source=spec["name"], category="check_script_failure", severity="ERROR",
            message=f"check tool {json.dumps(spec['name'], ensure_ascii=False)} exited with error ({err_text}): {err.strip() or raw or err_text}",
        ))
    return findings


def _exec_check(spec):
    started, seen, results = time.monotonic(), set(), []
    for d in target_dirs or [""]:
        for f in run_once(spec, d):
            key = (f["source"], f["category"], f["file"], f["line"], f["message"])
            if key not in seen:
                seen.add(key)
                if len(target_dirs) > 1 and not f["file"] and d:
                    f["message"] = f"[{d}] {f['message']}"
                results.append(f)
    return spec, results, time.monotonic() - started


builds_skipped = skip_build or os.environ.get("PLANTER_SKIP_BUILD", "").strip() not in ("", "0")
log(f"{len(enabled)} check(s) in {workdir}; target dirs: {' '.join(target_dirs) or '(none)'}; change base: {change_base}" + ("; builds skipped" if builds_skipped else ""))
max_workers = 1 if os.environ.get("PLANTER_SERIAL_CHECKS", "").strip() in ("1", "true") else min(8, os.cpu_count() or 4)

# Static checks run in parallel; build_verification, tests_json_parity and cutover_consistency NEVER
# run concurrently with static checks or with each other so tests_json_parity's tree/args.gn swap
# never races fx build.
static_specs = [s for s in enabled if s["name"] not in SERIAL_BUILD_CHECKS]
serial_specs = [s for s in enabled if s["name"] in SERIAL_BUILD_CHECKS]

try:
    diff_start = time.monotonic()
    if not only or "review_diff" in only or any(s["name"] == "visibility_audit" for s in enabled):
        precompute_review_diff()
    diff_duration = time.monotonic() - diff_start

    total, idx_map, results_by_idx, done_count = len(enabled), {id(s): i for i, s in enumerate(enabled)}, {}, 0
    if static_specs:
        with ThreadPoolExecutor(max_workers=max_workers) as executor:
            for fut in [executor.submit(_exec_check, spec) for spec in static_specs]:
                spec, findings, duration = fut.result()
                done_count += 1
                log(f"({done_count}/{total}) {spec['name']} done ({pcl.fmt_secs(duration)})")
                results_by_idx[idx_map[id(spec)]] = (spec, findings, duration)
    for spec in serial_specs:
        spec, findings, duration = _exec_check(spec)
        done_count += 1
        log(f"({done_count}/{total}) {spec['name']} done ({pcl.fmt_secs(duration)})")
        results_by_idx[idx_map[id(spec)]] = (spec, findings, duration)

    rows = [results_by_idx[i] for i in range(total)]
    all_findings = [f for _, flist, _ in rows for f in flist]

    if (not only or "review_diff" in only) and _precomputed_diff_findings is not None:
        # Same rule as planter's upload gate (mergeReviewDiffFindings): a review_diff finding is
        # skipped only when a check reported the same key at a severity that blocks at least as
        # much, or explicitly waived it in a waivable category. A non-blocking check finding never
        # hides a blocking review_diff finding.
        seen_blocking = {}
        waived = set()
        for f in all_findings:
            key = (f["category"], f["file"], f["line"], f["message"])
            seen_blocking[key] = seen_blocking.get(key, False) or pcl.is_blocking_finding(f)
            if f.get("waiver") and f["category"] in WAIVABLE_REVIEW_DIFF_CATEGORIES:
                waived.add(key)
        diff_findings = []
        for raw_f in _precomputed_diff_findings:
            f = dict(raw_f, source="review_diff", severity=pcl.normalize_severity(raw_f.get("severity", "")))
            key = (f["category"], f["file"], f["line"], f["message"])
            blocks = pcl.is_blocking_finding(f)
            if key in waived or (key in seen_blocking and (seen_blocking[key] or not blocks)):
                continue
            seen_blocking[key] = blocks
            diff_findings.append(f)
        rows.append(({"name": "review_diff", "enabled": True}, diff_findings, diff_duration))
        all_findings.extend(diff_findings)
except KeyboardInterrupt:
    log("interrupted")
    sys.exit(130)

# Annotate review_diff findings caused by unlinked or 0-byte working-tree build files so
# the coder gets consistent recovery or workspace_write_error instructions instead of
# misdiagnosing missing on-disk files as unmigrated GN targets.
def annotate_corrupted_worktree_review_diff(findings_list):
    has_write_err = any(f["category"] == "workspace_write_error" for f in findings_list)
    corrupted_dirs = {
        os.path.dirname(f["file"]).strip("/")
        for f in findings_list
        if f["category"] == "corrupted_working_tree_build_file" and f.get("file")
    }
    if not corrupted_dirs:
        return
    for f in findings_list:
        if f["source"] != "review_diff" or not pcl.is_blocking_finding(f):
            continue
        f_dir = os.path.dirname(f.get("file") or "").strip("/")
        if f_dir not in corrupted_dirs:
            continue
        if has_write_err:
            f["remediation"] = (
                f"Caused by missing or 0-byte working-tree build file(s) in '{f_dir}' while "
                "`workspace_write_error` prevents disk writes (`HEAD` still holds the "
                "committed files). Do not edit build files, run `git checkout`/`git restore`, "
                "or list `review_diff` in `CoderReport.disputed_findings` while "
                "`workspace_write_error` is active; keep `HEAD`'s changed files in "
                "`modified_files`, set `test_footers` to `[\"fx lint\"]` and "
                "`disputed_findings` to `[]`, and report `workspace_write_error` in `summary`."
            )
        else:
            f["remediation"] = (
                f"Caused by missing or 0-byte working-tree build file(s) in '{f_dir}' "
                "(see `migration_sanity/corrupted_working_tree_build_file`). Restore the "
                "working-tree build file(s) from `HEAD` via atomic sibling `tempfile.mkstemp` "
                "+ `os.write` + `os.replace` (never `open(..., \"w\")` or `git checkout --`), "
                "then re-run `run_checks.sh`."
            )


annotate_corrupted_worktree_review_diff(all_findings)

# Downgrade unamendable commit-message and corrupted working-tree findings when a
# workspace write error occurred.
def _downgrade_unwritable_workspace_findings(findings_list):
    if not any(f["category"] == "workspace_write_error" for f in findings_list):
        return
    unwritable_corrupted_rem = (
        "Workspace filesystem/mount write failure (`workspace_write_error`) prevents "
        "restoring working-tree build files on disk (`HEAD` still holds the committed "
        "files). Do not retry `tempfile.mkstemp`, `open(..., \"w\")`, "
        "`git checkout`/`git restore`, `fx bazel2gn`, or builds; keep `HEAD`'s changed "
        "files in `CoderReport.modified_files`, set `test_footers` to `[\"fx lint\"]` "
        "and `disputed_findings` to `[]`, and report `workspace_write_error` in "
        "`summary`."
    )
    for f in findings_list:
        if not pcl.is_blocking_finding(f):
            continue
        if f["source"] == "commit_message_format":
            f["severity"] = "INFO"
        elif f["category"] == "corrupted_working_tree_build_file":
            f["severity"] = "INFO"
            f["remediation"] = unwritable_corrupted_rem


_downgrade_unwritable_workspace_findings(all_findings)

blocking_findings = [f for f in all_findings if pcl.is_blocking_finding(f)]
exit_code = 1 if blocking_findings else 0

if as_json:
    out = [
        {k: f[k] for k in ("source", "category", "severity", "file", "line", "message", "remediation", "waiver") if k in ("source", "category", "severity", "message") or f.get(k)}
        for f in all_findings
    ]
    print(json.dumps(out, indent=2))
    sys.exit(exit_code)


def counts(findings):
    return ", ".join(f"{n} {sev}" for sev in ("ERROR", "WARNING", "INFO") for n in [sum(1 for f in findings if f["severity"] == sev)] if n)


def print_finding(prefix, f):
    where = f["file"] + (f":{f['line']}" if f["file"] and f["line"] else "")
    print(f"{prefix}[{f['severity']}] {f['source']}/{f['category'] or '-'}" + (f"  {where}" if where else ""))
    for line in f["message"].splitlines() or [""]:
        print("    " + line)
    if f.get("waiver"):
        print("    Waiver: " + f["waiver"])
    elif f["remediation"]:
        rem = f["remediation"].splitlines()
        print("    Remediation: " + rem[0])
        for line in rem[1:]:
            print("    " + line)


width = max([len(s["name"]) for s, _, _ in rows] + [len(s["name"]) for s in disabled] + [4])
print("Checks (planter runs these, in this order, after you report):")
for spec, findings, secs in rows:
    status = "FAIL" if any(pcl.is_blocking_finding(f) for f in findings) else "PASS"
    detail = counts(findings)
    print(f"  {status}  {spec['name']:<{width}}  {pcl.fmt_secs(secs):>7}" + (f"  {detail}" if detail else ""))
for spec in disabled:
    print(f"  ----  {spec['name']:<{width}}  disabled in checks/manifest.json (planter skips it)")
if not rows:
    print("  (no enabled checks)")

if blocking_findings:
    print(f"\nBlocking findings ({len(blocking_findings)}); planter sends the change back for any of these:\n")
    for n, f in enumerate(blocking_findings, 1):
        print_finding(f"{n}. ", f)
        print()
others = [f for f in all_findings if not pcl.is_blocking_finding(f)]
if others:
    print(f"\nNon-blocking informational/waived notes ({len(others)}) -- already satisfied, do NOT dispute:\n")
    for f in others:
        print_finding("- ", f)
        print()

print()
def _missing_agg_disputable_dirs(findings_list):
    return {
        os.path.dirname(f["file"]).strip("/")
        for f in findings_list
        if f.get("source") == "cq_reachability"
        and f.get("category") == "unexported_bazel_test"
        and "`CoderReport.disputed_findings`" in (f.get("remediation") or "")
        and f.get("file")
    }


def _dir_in_any_subtree(sub_dir, parent_dirs):
    return any(
        pd == "" or sub_dir == pd or sub_dir.startswith(pd + "/")
        for pd in parent_dirs
    )


# Check whether a finding is an expected human-disputable test-suite finding
# (missing area aggregation test_suite() in Case 1/Case 2 or expanded empty test_suite()).
def _is_missing_agg_disputable_finding(f, findings_list):
    rem = f.get("remediation") or ""
    if (
        f.get("source") == "cq_reachability"
        and f.get("category") == "unexported_bazel_test"
        and "`CoderReport.disputed_findings`" in rem
    ):
        return True
    if (
        f.get("source") == "test_suite_layout"
        and f.get("category") == "expanded_empty_test_suite"
    ):
        return True
    missing_dirs = _missing_agg_disputable_dirs(findings_list)
    parity_cats = (
        "test_dropped_from_tests_json",
        "dual_build_bazel_variant_missing",
    )
    if (
        missing_dirs
        and f.get("source") == "tests_json_parity"
        and f.get("category") in parity_cats
    ):
        f_path = f.get("file") or ""
        if f_path:
            return _dir_in_any_subtree(
                os.path.dirname(f_path).strip("/"), missing_dirs
            )
        other_dropped_dirs = {
            os.path.dirname(g["file"]).strip("/")
            for g in findings_list
            if g.get("source") == "tests_json_parity"
            and g.get("category") in parity_cats
            and g.get("file")
            and not _dir_in_any_subtree(
                os.path.dirname(g["file"]).strip("/"), missing_dirs
            )
        }
        return not other_dropped_dirs
    return False


# Format deterministic check script blocking items for the run_checks.sh summary footer.
def _format_script_blocking_summary(findings_list):
    if any(f.get("category") == "workspace_write_error" for f in findings_list):
        return None
    script_blocking = [
        f
        for f in findings_list
        if f.get("source")
        and f.get("source") != "review_diff"
        and not _is_missing_agg_disputable_finding(f, findings_list)
    ]
    if not script_blocking:
        return None
    item_labels = sorted(
        {
            f"`{f['source']}/{f['category']}` (`{f['file']}`)"
            if f.get("file")
            else f"`{f['source']}/{f['category']}`"
            for f in script_blocking
        }
    )
    premature_note = ""
    if any(
        f.get("category") == "premature_pre_upload_git_commit"
        for f in script_blocking
    ):
        premature_note = (
            " Run `git reset HEAD~1` (without `--hard`) to move the local "
            "commit back into the uncommitted working tree before re-running "
            "`run_checks.sh`: Planter's host upload gate keeps "
            "`PLANTER_CHANGE_BASE=HEAD` without `TAG: planter`, so leaving "
            "`HEAD` committed fails upload with `empty_migration_diff`, and "
            "`premature_pre_upload_git_commit` must never be disputed."
        )
    extra = ""
    if any(_is_missing_agg_disputable_finding(f, findings_list) for f in findings_list):
        extra = (
            " Once those fixable findings pass, report any remaining "
            "human-verification (`expanded_empty_test_suite`) or "
            "missing-aggregation-suite finding(s) in "
            "`CoderReport.disputed_findings` and `summary`."
        )
    return (
        f"ACTION REQUIRED: fix all {len(script_blocking)} deterministic check "
        f"finding(s) ({', '.join(item_labels)}) in the workspace and re-run "
        "`run_checks.sh` BEFORE submitting `CoderReport` (including on rebased or "
        "follow-up patchsets). Do NOT stop early or submit `disputed_findings` "
        "while any fixable deterministic check script reports "
        f"ERROR/WARNING.{premature_note}{extra}"
    )


# Format the clean-pass footer, distinguishing an intentionally unmigrated
# blocked or upstream-relocated target (where Planter's post-turn upload gate
# emits expected empty_migration_diff) from a normal non-empty migration diff.
def _format_clean_pass_summary(findings_list, changed_raw):
    has_changed = bool((changed_raw or "").strip())
    is_blocked_unmigrated = any(
        f.get("category")
        in (
            "target_blocked_by_unmigrated_deps",
            "target_directory_missing",
            "target_already_migrated",
        )
        for f in findings_list
    )
    if not has_changed and is_blocked_unmigrated:
        return (
            "RESULT: PASS: no blocking findings in live `$PLANTER_PROJECT_DIR` "
            "checks (target directory is already migrated at the change base, "
            "blocked by unmigrated upstream dependencies, or no longer exists "
            "in the checkout, and has no "
            "migration diff). Keep `modified_files` set to `[]`, `test_footers` "
            "set to `[\"fx lint\"]`, and `disputed_findings` set to `[]` in "
            "`CoderReport` (including on retry rounds). Planter's post-turn "
            "upload gate emits `migration_sanity/empty_migration_diff` whenever "
            "`modified_files` is empty so no empty CL is uploaded before halting "
            "the task; `empty_migration_diff` is the expected host empty-CL "
            "guard, so NEVER add `empty_migration_diff` "
            "to `disputed_findings`."
        )
    if not has_changed:
        return (
            "RESULT: PASS: no blocking findings in live `$PLANTER_PROJECT_DIR` "
            "checks (working tree currently has no modified files). If leaving "
            "a blocked or pre-existing broken target unmigrated on purpose, "
            "keep `modified_files: []`, `test_footers: [\"fx lint\"]`, and "
            "`disputed_findings: []` (post-turn "
            "`migration_sanity/empty_migration_diff` is Planter's expected "
            "empty-CL halt guard and must NEVER be "
            "disputed). Otherwise author the migration edits in "
            "`$PLANTER_WORKDIR` and re-run `run_checks.sh` before reporting."
        )
    return (
        "RESULT: PASS: no blocking findings in live `$PLANTER_PROJECT_DIR` "
        "checks. Set `disputed_findings` to `[]` in `CoderReport` (drop any "
        "prior-patchset or Gerrit thread disputes for checks that now pass; "
        "never run in-tree `build/beads/` check scripts directly)."
    )


has_workspace_write_error = any(
    f["category"] == "workspace_write_error" for f in blocking_findings
)
if blocking_findings:
    sources = sorted({f["source"] for f in blocking_findings})
    print(f"RESULT: FAIL: {len(blocking_findings)} blocking finding(s) ({counts(blocking_findings)}) from {', '.join(sources)}.")
    script_action_note = _format_script_blocking_summary(blocking_findings)
    if script_action_note:
        print(script_action_note)
    missing_agg_findings = [
        f for f in blocking_findings if _is_missing_agg_disputable_finding(f, blocking_findings)
    ]
    # Guide CoderReport.disputed_findings when all fixable check scripts passed
    # and only disputable check findings (missing-aggregation test findings or
    # expanded_empty_test_suite) and/or review_diff WARNINGs remain.
    if (
        not script_action_note
        and not has_workspace_write_error
        and (
            missing_agg_findings
            or all(
                f["source"] == "review_diff" and f["severity"] == "WARNING"
                for f in blocking_findings
            )
        )
    ):
        dispute_parts = []
        if missing_agg_findings:
            agg_labels = ", ".join(
                sorted(
                    {
                        f"`{f['source']}/{f['category']}` (`{f['file']}`)"
                        if f.get("file")
                        else f"`{f['source']}/{f['category']}`"
                        for f in missing_agg_findings
                    }
                )
            )
            dispute_parts.append(
                f"human-review / missing-aggregation-suite test finding(s) ({agg_labels})"
            )
        diff_warnings = []
        seen_diff_warnings = set()
        for f in blocking_findings:
            if f["source"] == "review_diff":
                k = (f.get("category", ""), f.get("file", ""))
                if k not in seen_diff_warnings:
                    seen_diff_warnings.add(k)
                    diff_warnings.append(k)
        if diff_warnings:
            warn_str = ", ".join(
                f"`review_diff/{cat}` (`{p}`)" if p else f"`review_diff/{cat}`"
                for cat, p in diff_warnings
            )
            dispute_parts.append(f"`review_diff` WARNING(s) ({warn_str})")
        # Never instruct the coder to emit disputed_findings during partial preview runs.
        if builds_skipped or only:
            print(
                "NOTE: partial check preview only (`--skip-build` / `--only`): only "
                + " and ".join(dispute_parts)
                + " remain in the checks run so far, but build verification and "
                "commit-message checks were skipped. Do NOT submit `CoderReport` or "
                "populate `disputed_findings` after a `--skip-build` or `--only` run: "
                "finish targeted build/test verification, amend `HEAD` with `Test:` "
                "and `Bazel-Migration-Target:` footers when `HEAD` carries `TAG: planter` "
                "(otherwise keep Git edits uncommitted), and run full `run_checks.sh` "
                "(with no flags) before reporting any remaining `disputed_findings`."
            )
        else:
            print(
                "NOTE: all fixable deterministic check scripts passed; only "
                + " and ".join(dispute_parts)
                + " remain. Ensure the package `test_suite()` is defined with explicit "
                "`tests = [...]` (omitting `visibility` until the aggregation package is "
                "known when unexported; never add a per-package `bazel_test_suite` or "
                "guess an aggregation directory) and list each remaining item in "
                "`CoderReport.disputed_findings` (plus `summary`: for "
                "`expanded_empty_test_suite`, request that a human reviewer ensure that "
                "all relevant tests in the package have been included and state that "
                "agents must not resolve that comment; for `unexported_bazel_test`, ask "
                "the reviewer which aggregation `test_suite()` -- or where a new one, "
                "with a matching `bazel_test_suite` in `BUILD.gn`, should be added if "
                "none exists -- the package's `test_suite()`(s) should be added to, "
                "suggesting asking a member of //build/bazel/BAZEL_OWNERS if they do not know). Drop stale "
                "`disputed_findings` entries for checks that now pass (never drop "
                "`test_suite_layout/expanded_empty_test_suite` while the `test_suite()` "
                "expansion remains in the change). Each `reason` in "
                "`disputed_findings` is posted directly to the human reviewer as a "
                "Gerrit review thread, so write a short, factual message addressed to "
                "a human."
            )
else:
    print(
        _format_clean_pass_summary(
            all_findings, os.environ.get("PLANTER_CHANGED_FILES", "")
        )
    )
if any(f["category"] == "workspace_write_error" for f in blocking_findings):
    print(
        "NOTE: workspace filesystem/mount write failure detected (`workspace_write_error`). "
        "Stop retrying build or `git commit` write commands, keep `HEAD~1..HEAD` changed "
        "files in `CoderReport.modified_files` when `HEAD` already holds this task's local "
        "migration commit (never `[]`, which triggers `empty_migration_diff` when "
        "`PLANTER_CHANGE_BASE=HEAD`), set `test_footers` to `[\"fx lint\"]` and "
        "`disputed_findings` to `[]` in `CoderReport` (empty `test_footers` fails "
        "`test_verifier` with `missing_test_footer`, while target-bearing `Test:` commands "
        "trigger spurious query errors on an unwritable mount), and report the mount write "
        "failure in `summary`."
    )
if any(f["category"] == "rbe_auth_error" for f in blocking_findings):
    print(
        "NOTE: remote build (RBE) authentication failure detected (`rbe_auth_error`). "
        "Do not retry failing RBE `fx build` or `fx bazel` commands; fix all static "
        "check findings, keep pre-upload Git edits uncommitted in the working tree "
        "(or `git commit --amend` only when `HEAD` already carries `TAG: planter`), "
        "keep all changed files in `CoderReport.modified_files`, and note the RBE "
        "credential error in `summary`."
    )
if only:
    print("NOTE: only some checks ran (--only). Run all of them before reporting.")
if any(f["category"] == "build_skipped" for f in all_findings):
    print("NOTE: the builds were skipped. Run `run_checks.sh` without --skip-build before reporting.")
sys.exit(exit_code)
PYEOF
