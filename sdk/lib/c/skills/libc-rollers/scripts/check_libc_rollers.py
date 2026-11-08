#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Status, triage, and retrigger tool for Fuchsia libc rollers.

Dynamically discovers the LLVM-based libc repositories from //sdk/lib/c/manual-roll.sh
and resolves their current Buildbucket builders and LUCI Scheduler job IDs on the fly
from roller names via Buildbucket build tags, live LUCI Scheduler APIs, and
//integration/infra/config.
"""

import argparse
import base64
import datetime
import hmac
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import traceback
import urllib.request
import uuid
import xml.etree.ElementTree as ET
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, quote, urlparse

# BEGIN AUTO-GENERATED DEFAULT_ROLLERS
DEFAULT_ROLLERS = {
    "llvm-libc-roller": {
        "bucket": "global.roller",
        "manifest_project": "llvm-project/libc",
        "remote": "https://llvm.googlesource.com/llvm-project/libc",
        "path": "third_party/llvm-libc/src",
        "schedule": "with 4h interval",
    },
    "scudo-roller": {
        "bucket": "global.roller",
        "manifest_project": "scudo",
        "remote": "https://llvm.googlesource.com/scudo",
        "path": "third_party/scudo/src",
        "schedule": "0 5 * * *",
    },
    "gwp_asan-roller": {
        "bucket": "global.roller",
        "manifest_project": "gwp-asan",
        "remote": "https://llvm.googlesource.com/gwp-asan",
        "path": "third_party/scudo/gwp_asan",
        "schedule": "0 4 * * *",
    },
}
# END AUTO-GENERATED DEFAULT_ROLLERS


def parse_default_luci_project(fuchsia_root: Path) -> str:
    """Extract the roller LUCI project name from //sdk/lib/c/README.md if present."""
    readme = fuchsia_root / "sdk" / "lib" / "c" / "README.md"
    if readme.exists():
        m = re.search(
            r"\[LUCI\]:\s*https://[^\s]+/p/([^/\s]+)/",
            readme.read_text(),
        )
        if m:
            return m.group(1)
    return "fuchsia"


MANIFEST_URL = (
    "https://fuchsia.googlesource.com/fuchsia/+/refs/heads/main/"
    "manifests/third_party/all?format=TEXT"
)
SCHEDULER_PRPC_BASE = (
    "https://luci-scheduler.appspot.com/prpc/scheduler.Scheduler"
)


def find_fuchsia_root(explicit_dir: str | None = None) -> Path:
    """Locate the Fuchsia source checkout root."""
    if explicit_dir:
        return Path(explicit_dir).resolve()
    env_dir = os.environ.get("FUCHSIA_DIR")
    if env_dir and (Path(env_dir) / ".jiri_root").exists():
        return Path(env_dir).resolve()
    curr = Path(__file__).resolve().parent
    for parent in [curr, *curr.parents]:
        if (parent / "prebuilt" / "tools" / "buildbucket" / "bb").exists():
            return parent
    return Path.cwd().resolve()


def resolve_checkout_and_worktree_dirs(
    explicit_fuchsia_dir: str | None, explicit_worktree_dir: str | None
) -> tuple[Path, Path]:
    """Resolve the reference Fuchsia checkout and dedicated Jiri worktree paths."""
    detected_root = find_fuchsia_root(explicit_fuchsia_dir)
    if explicit_fuchsia_dir and explicit_worktree_dir:
        return (
            Path(explicit_fuchsia_dir).resolve(),
            Path(explicit_worktree_dir).resolve(),
        )

    dot_git = detected_root / ".git"
    if not explicit_fuchsia_dir and dot_git.is_file():
        try:
            git_text = dot_git.read_text(encoding="utf-8").strip()
            m = re.match(
                r"^gitdir:\s*(.+)/\.git/worktrees/[^/]+$",
                git_text,
            )
            if m:
                main_checkout = (detected_root / m.group(1)).resolve()
                wt = (
                    Path(explicit_worktree_dir).resolve()
                    if explicit_worktree_dir
                    else detected_root
                )
                return main_checkout, wt
        except OSError:
            pass

    wt = (
        Path(explicit_worktree_dir).resolve()
        if explicit_worktree_dir
        else detected_root.parent / f"{detected_root.name}-libc-rollers"
    )
    return detected_root, wt


def parse_manual_roll_repos(fuchsia_root: Path) -> dict[str, str]:
    """Parse REPOS=([path]=manifest_project ...) from //sdk/lib/c/manual-roll.sh."""
    manual_roll = fuchsia_root / "sdk" / "lib" / "c" / "manual-roll.sh"
    if not manual_roll.exists():
        return {
            v["path"]: v["manifest_project"] for v in DEFAULT_ROLLERS.values()
        }
    text = manual_roll.read_text()
    m = re.search(r"declare\s+-r\s+-A\s+REPOS=\((.*?)\)", text, re.DOTALL)
    if not m:
        return {
            v["path"]: v["manifest_project"] for v in DEFAULT_ROLLERS.values()
        }
    repos = {}
    for entry in re.finditer(r"\[([^\]]+)\]=([^\s\n]+)", m.group(1)):
        repos[entry.group(1).strip()] = entry.group(2).strip()
    return repos or {
        v["path"]: v["manifest_project"] for v in DEFAULT_ROLLERS.values()
    }


def parse_luci_scheduler_configs(fuchsia_root: Path) -> dict[str, dict]:
    """Discover roller jobs/builders by parsing generated luci-scheduler.cfg files."""
    gen_root = fuchsia_root / "integration" / "infra" / "config" / "generated"
    if not gen_root.exists():
        return {}

    # Map gitiles repo URL -> (luci_project, job_id)
    repo_to_job: dict[str, tuple[str, str]] = {}
    # Map (luci_project, job_id) -> job metadata
    jobs_meta: dict[tuple[str, str], dict] = {}

    for cfg_path in sorted(gen_root.glob("*/luci/luci-scheduler.cfg")):
        luci_project = cfg_path.parent.parent.name
        try:
            content = cfg_path.read_text()
        except OSError:
            continue

        for trig_match in re.finditer(
            r"trigger\s*\{(.*?)\n\}", content, re.DOTALL
        ):
            block = trig_match.group(1)
            repos = re.findall(r'repo:\s*"([^"]+)"', block)
            triggers = re.findall(r'triggers:\s*"([^"]+)"', block)
            for r_url in repos:
                for t_job in triggers:
                    if "roller" in t_job and "deployer" not in t_job:
                        repo_to_job[r_url.rstrip("/")] = (luci_project, t_job)

        for job_match in re.finditer(r"job\s*\{(.*?)\n\}", content, re.DOTALL):
            block = job_match.group(1)
            jid_m = re.search(r'^\s*id:\s*"([^"]+)"', block, re.MULTILINE)
            if not jid_m:
                continue
            jid = jid_m.group(1)
            sched_m = re.search(
                r'^\s*schedule:\s*"([^"]+)"', block, re.MULTILINE
            )
            bb_m = re.search(r"buildbucket\s*\{(.*?)\}", block, re.DOTALL)
            if not bb_m:
                continue
            bb_block = bb_m.group(1)
            bucket_m = re.search(r'bucket:\s*"([^"]+)"', bb_block)
            builder_m = re.search(r'builder:\s*"([^"]+)"', bb_block)
            if bucket_m and builder_m:
                jobs_meta[(luci_project, jid)] = {
                    "luci_project": luci_project,
                    "scheduler_job": jid,
                    "schedule": sched_m.group(1) if sched_m else "triggered",
                    "bucket": bucket_m.group(1),
                    "builder_name": builder_m.group(1),
                    "builder": f"{luci_project}/{bucket_m.group(1)}/{builder_m.group(1)}",
                }

    discovered_by_remote = {}
    for r_url, key in repo_to_job.items():
        if key in jobs_meta:
            discovered_by_remote[r_url] = jobs_meta[key]
    return discovered_by_remote


def get_luci_token(fuchsia_root: Path) -> str:
    """Obtain a LUCI auth token using the prebuilt luci-auth binary."""
    luci_auth = fuchsia_root / "prebuilt" / "tools" / "luci-auth" / "luci-auth"
    res = subprocess.run(
        [str(luci_auth), "token"],
        capture_output=True,
        text=True,
        check=True,
    )
    return res.stdout.strip()


def scheduler_rpc(token: str, method: str, payload: dict) -> dict:
    """Call a LUCI Scheduler pRPC method."""
    req = urllib.request.Request(
        f"{SCHEDULER_PRPC_BASE}/{method}",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
        method="POST",
    )
    raw = urllib.request.urlopen(req, timeout=15).read().decode("utf-8")
    if raw.startswith(")]}"):
        raw = raw.split("\n", 1)[1]
    return json.loads(raw)


def resolve_live_scheduler_jobs(
    fuchsia_root: Path, rollers: dict[str, dict]
) -> None:
    """Query live LUCI Scheduler GetJobs so changing hex suffixes are always fresh."""
    try:
        token = get_luci_token(fuchsia_root)
    except Exception:
        return

    projects = {info["luci_project"] for info in rollers.values()}
    live_jobs_by_project: dict[str, list[str]] = {}
    for proj in projects:
        try:
            data = scheduler_rpc(token, "GetJobs", {"project": proj})
            live_jobs_by_project[proj] = [
                j["jobRef"]["job"]
                for j in data.get("jobs", [])
                if "jobRef" in j and "job" in j["jobRef"]
            ]
        except Exception:
            continue

    for roller_name, info in rollers.items():
        proj = info["luci_project"]
        live_list = live_jobs_by_project.get(proj, [])
        if not live_list or info["scheduler_job"] in live_list:
            continue
        # Match `<bucket>-<roller_name>` with any `-<hex>` suffix
        prefix = f"{info['bucket']}-{roller_name}"
        pattern = re.compile(rf"^{re.escape(prefix)}(?:-[0-9a-f]+)?$")
        matched = False
        for candidate in live_list:
            if pattern.match(candidate):
                info["scheduler_job"] = candidate
                matched = True
                break
        if not matched:
            any_bucket_pat = re.compile(
                rf"^([a-z0-9_.]+)-{re.escape(roller_name)}(?:-[0-9a-f]+)?$"
            )
            for candidate in live_list:
                m = any_bucket_pat.match(candidate)
                if m:
                    info["bucket"] = m.group(1)
                    info["builder"] = f"{proj}/{info['bucket']}/{roller_name}"
                    info["scheduler_job"] = candidate
                    break


def discover_rollers_and_pending(
    fuchsia_root: Path,
) -> tuple[dict[str, dict], dict[str, dict]]:
    """Dynamically discover roller configs and pending upstream commits."""
    manifest_b64 = urllib.request.urlopen(MANIFEST_URL, timeout=15).read()
    manifest_xml = base64.b64decode(manifest_b64).decode("utf-8")
    root = ET.fromstring(manifest_xml)

    path_to_proj = parse_manual_roll_repos(fuchsia_root)
    proj_to_path = {proj: path for path, proj in path_to_proj.items()}
    sched_by_remote = parse_luci_scheduler_configs(fuchsia_root)

    default_proj = parse_default_luci_project(fuchsia_root)
    # Index fallback defaults by manifest_project
    fallback_by_proj = {
        v["manifest_project"]: (
            k,
            {
                **v,
                "luci_project": default_proj,
                "builder": f"{default_proj}/{v['bucket']}/{k}",
                "scheduler_job": f"{v['bucket']}-{k}",
            },
        )
        for k, v in DEFAULT_ROLLERS.items()
    }

    rollers: dict[str, dict] = {}
    pending: dict[str, dict] = {}

    for proj_el in root.findall("./projects/project"):
        proj_name = proj_el.attrib.get("name")
        if proj_name not in proj_to_path:
            continue
        rev = proj_el.attrib["revision"]
        remote = proj_el.attrib["remote"].rstrip("/")
        rel_path = proj_el.attrib.get("path", proj_to_path[proj_name])

        short_name = f"{proj_name.split('/')[-1]}-roller"
        fb_name, fb_info = fallback_by_proj.get(
            proj_name,
            (
                short_name,
                {
                    "luci_project": default_proj,
                    "bucket": "global.roller",
                    "builder": f"{default_proj}/global.roller/{short_name}",
                    "scheduler_job": f"global.roller-{short_name}",
                    "manifest_project": proj_name,
                    "remote": remote,
                    "path": rel_path,
                    "schedule": "triggered",
                },
            ),
        )

        cfg_meta = sched_by_remote.get(remote, {})
        roller_name = cfg_meta.get("builder_name", fb_name)
        rollers[roller_name] = {
            "luci_project": cfg_meta.get(
                "luci_project", fb_info["luci_project"]
            ),
            "bucket": cfg_meta.get("bucket", fb_info["bucket"]),
            "builder": cfg_meta.get("builder", fb_info["builder"]),
            "scheduler_job": cfg_meta.get(
                "scheduler_job", fb_info["scheduler_job"]
            ),
            "manifest_project": proj_name,
            "remote": remote,
            "path": rel_path,
            "schedule": cfg_meta.get("schedule", fb_info["schedule"]),
        }

        log_url = f"{remote}/+log/{rev}..refs/heads/main?format=JSON"
        raw = urllib.request.urlopen(log_url, timeout=15).read().decode("utf-8")
        if raw.startswith(")]}"):
            raw = raw.split("\n", 1)[1]
        data = json.loads(raw)
        commits = data.get("log", [])
        head_rev = commits[0]["commit"] if commits else rev
        parsed_commits = []
        commit_epochs: list[float] = []
        for c in commits:
            t_str = c.get("committer", {}).get("time", "")
            c_epoch: float | None = None
            if t_str:
                try:
                    c_epoch = datetime.datetime.strptime(
                        t_str, "%a %b %d %H:%M:%S %Y %z"
                    ).timestamp()
                    commit_epochs.append(c_epoch)
                except ValueError:
                    pass
            parsed_commits.append(
                {
                    "commit": c["commit"],
                    "short_sha": c["commit"][:12],
                    "subject": c["message"].splitlines()[0],
                    "author": c.get("author", {}).get("email", ""),
                    "time": t_str,
                    "epoch": c_epoch,
                }
            )
        pending[roller_name] = {
            "project": proj_name,
            "remote": remote,
            "pinned_revision": rev,
            "upstream_head": head_rev,
            "pending_count": len(commits),
            "oldest_pending_epoch": (
                commit_epochs[-1] if commit_epochs else None
            ),
            "newest_pending_epoch": (
                commit_epochs[0] if commit_epochs else None
            ),
            "pending_commits": parsed_commits,
        }

    resolve_live_scheduler_jobs(fuchsia_root, rollers)
    return rollers, pending


def _decode_json_stream(raw: str) -> list[dict]:
    """Decode newline/whitespace-delimited JSON objects emitted by bb ls -json."""
    items = []
    decoder = json.JSONDecoder()
    raw = raw.strip()
    pos = 0
    while pos < len(raw):
        while pos < len(raw) and raw[pos].isspace():
            pos += 1
        if pos >= len(raw):
            break
        try:
            obj, idx = decoder.raw_decode(raw[pos:])
            items.append(obj)
            pos += idx
        except json.JSONDecodeError:
            break
    return items


def fetch_recent_builds(
    fuchsia_root: Path, builder: str, count: int = 3
) -> list[dict]:
    """Run `bb ls` for a single roller builder and return parsed JSON records."""
    bb_bin = fuchsia_root / "prebuilt" / "tools" / "buildbucket" / "bb"
    res = subprocess.run(
        [
            str(bb_bin),
            "ls",
            "-n",
            str(count),
            "-json",
            "-fields",
            "id,status,create_time,end_time,summary_markdown,tags",
            builder,
        ],
        capture_output=True,
        text=True,
        check=True,
    )
    return _decode_json_stream(res.stdout)


def fetch_last_failed_build(
    fuchsia_root: Path, builder: str, recent_builds: list[dict]
) -> dict | None:
    """Return the most recent failed build, querying bb ls -status failure if needed."""
    for b in recent_builds:
        if b.get("status") in ("FAILURE", "INFRA_FAILURE"):
            return b
    bb_bin = fuchsia_root / "prebuilt" / "tools" / "buildbucket" / "bb"
    res = subprocess.run(
        [
            str(bb_bin),
            "ls",
            "-n",
            "1",
            "-status",
            "failure",
            "-json",
            "-fields",
            "id,status,create_time,end_time,summary_markdown,tags",
            builder,
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if res.returncode != 0:
        return None
    failed = _decode_json_stream(res.stdout)
    return failed[0] if failed else None


def update_scheduler_job_from_builds(
    roller_info: dict, builds: list[dict]
) -> None:
    """Extract scheduler_job_id tag or Retrigger URL from recent builds if newer."""
    for b in builds:
        for t in b.get("tags", []):
            if t.get("key") == "scheduler_job_id" and "/" in t.get("value", ""):
                proj, job_id = t["value"].split("/", 1)
                roller_info["luci_project"] = proj
                roller_info["scheduler_job"] = job_id
                return
        summary = b.get("summaryMarkdown", "")
        m = re.search(
            r"https://luci-scheduler\.appspot\.com/jobs/([^/\s\)]+)/([^/\s\)]+)",
            summary,
        )
        if m:
            roller_info["luci_project"] = m.group(1)
            roller_info["scheduler_job"] = m.group(2)
            return


def inspect_roll_build_steps(fuchsia_root: Path, build_id: str) -> dict:
    """Inspect build steps for Gerrit CL/patchset, commit count, revision, and whether it restored a prior CL."""
    bb_bin = fuchsia_root / "prebuilt" / "tools" / "buildbucket" / "bb"
    res = subprocess.run(
        [str(bb_bin), "get", str(build_id), "-steps"],
        capture_output=True,
        text=True,
        check=False,
    )
    info: dict = {
        "cl_num": None,
        "patchset": None,
        "commit_count": None,
        "roll_revision": None,
        "restored_previous_attempt": False,
    }
    if res.returncode != 0:
        return info

    out = res.stdout
    matches = re.findall(
        r"change%22%3A\+%22(\d+)%22%2C\+%22patchset%22%3A\+%22(\d+)%22",
        out,
    )
    if matches:
        info["cl_num"], info["patchset"] = matches[-1]
    else:
        m_cl = re.findall(
            r"https://fuchsia-review\.googlesource\.com/(?:c/fuchsia/\+/)?(\d+)",
            out,
        )
        if m_cl:
            info["cl_num"] = m_cl[-1]

    m_commits = re.search(
        r'Step "get gerrit change numbers for [^|]+\|(\d+) commits?"',
        out,
    )
    if m_commits:
        info["commit_count"] = int(m_commits.group(1))

    m_rev = re.search(r'Step "roll ([0-9a-f]{12,40})"', out)
    if m_rev:
        info["roll_revision"] = m_rev.group(1)

    has_restore_step = 'Step "restore CL from previous roll attempt"' in out
    ps_int = (
        int(info["patchset"])
        if info["patchset"] and str(info["patchset"]).isdigit()
        else 1
    )
    info["restored_previous_attempt"] = has_restore_step or ps_int > 1
    return info


def describe_roll_attempt(
    step_info: dict, prev_step_info: dict | None = None
) -> str:
    """Format whether a roll run restored a previous roll attempt or is a new roll with more commits."""
    count = step_info.get("commit_count")
    rev = (step_info.get("roll_revision") or "")[:12]
    ps = step_info.get("patchset")
    rev_part = f" to {rev}" if rev else ""

    if step_info.get("restored_previous_attempt"):
        count_part = (
            f"same {count} commit(s){rev_part}"
            if count is not None
            else f"same commit{rev_part}"
        )
        ps_part = f", patchset {ps}" if ps else ""
        return f"restored previous roll attempt ({count_part}{ps_part})"

    prev_count = prev_step_info.get("commit_count") if prev_step_info else None
    if (
        count is not None
        and prev_count is not None
        and count > prev_count
        and step_info.get("roll_revision")
        != prev_step_info.get("roll_revision")
    ):
        delta = count - prev_count
        return (
            f"new roll (+{delta} commit(s) over previous failed run: "
            f"{count} vs {prev_count} commit(s){rev_part})"
        )
    if count is not None:
        return f"new roll ({count} commit(s){rev_part})"
    if rev or step_info.get("cl_num"):
        return f"new roll{rev_part}"
    return ""


def _first_meaningful_summary_line(summary_md: str) -> str:
    """Return a concise single-line summary of a Buildbucket summaryMarkdown, including multi-test bullet items."""
    lines = [
        line.strip() for line in (summary_md or "").splitlines() if line.strip()
    ]
    lines = [line for line in lines if not line.startswith("```")]
    if not lines:
        return ""
    is_false_rejection = any(
        "false rejection" in line.lower() for line in lines[:3]
    )
    for idx, line in enumerate(lines):
        if "original failure continues below" in line.lower() and idx + 1 < len(
            lines
        ):
            lines = lines[idx + 1 :]
            break
    first = lines[0]
    suffix = " [false rejection]" if is_false_rejection else ""
    if re.match(r"^\d+\s+tests?\s+failed:?$", first, re.IGNORECASE):
        bullets = []
        for extra in lines[1:]:
            if extra.startswith(("-", "*")):
                item = extra.lstrip("-* ").strip()
                m_pkg = re.search(
                    r"fuchsia-pkg://[^/\s`]+/([^#\s`]+)(?:#meta/([^\s`]+))?",
                    item,
                )
                if m_pkg:
                    pkg, comp = m_pkg.group(1), m_pkg.group(2)
                    if comp and comp not in (
                        "integration_test.cm",
                        "test-root.cm",
                        "default.cm",
                    ):
                        bullets.append(f"`{comp}`")
                    elif comp:
                        bullets.append(f"`{pkg}#meta/{comp}`")
                    else:
                        bullets.append(f"`{pkg}`")
                else:
                    bullets.append(item)
        if bullets:
            shown = ", ".join(bullets[:3])
            if len(bullets) > 3:
                shown += f", +{len(bullets) - 3} more"
            return f"{first.rstrip(':')}: {shown}{suffix}"
    return f"{first}{suffix}" if suffix not in first else first


def extract_gerrit_url_from_summary(summary_md: str) -> str | None:
    """Extract the fallback Gerrit URL from a roller build's summaryMarkdown."""
    m = re.search(
        r"https://fuchsia-review\.googlesource\.com/[^\s\)]+",
        summary_md or "",
    )
    return m.group(0) if m else None


def fetch_cl_builds(
    fuchsia_root: Path, change_num: str, patchset: str | None = None
) -> tuple[str, list[dict], str | None]:
    """Return (cl_url, builds, error) for a Gerrit CL from Buildbucket."""
    bb_bin = fuchsia_root / "prebuilt" / "tools" / "buildbucket" / "bb"
    cl_url = f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{change_num}"
    if patchset:
        cl_url += f"/{patchset}"
    else:
        cl_url += "/1"

    res = subprocess.run(
        [str(bb_bin), "ls", "-cl", cl_url, "-json"],
        capture_output=True,
        text=True,
        check=False,
    )
    if res.returncode != 0:
        return cl_url, [], res.stderr.strip()
    return cl_url, _decode_json_stream(res.stdout), None


def run_triage_cl(
    fuchsia_root: Path,
    change_num: str,
    patchset: str | None = None,
    builds: list[dict] | None = None,
) -> tuple[str, str]:
    """Query Buildbucket for all tryjobs on a Gerrit CL and return (report_md, brief_summary)."""
    if builds is None:
        cl_url, builds, err = fetch_cl_builds(
            fuchsia_root, change_num, patchset
        )
        if err is not None:
            return err, f"CL {cl_url}: {err}"
    else:
        cl_url = (
            f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{change_num}"
            + (f"/{patchset}" if patchset else "/1")
        )

    lines = [f"# CI Triage Report for CL: {cl_url}", ""]
    if not builds:
        lines.append("No builds found for this CL on Buildbucket.")
        return "\n".join(lines), f"CL {cl_url}: no tryjob builds found"

    success = [b for b in builds if b.get("status") == "SUCCESS"]
    failed = [
        b for b in builds if b.get("status") in ("FAILURE", "INFRA_FAILURE")
    ]
    running = [b for b in builds if b.get("status") in ("STARTED", "SCHEDULED")]

    lines.append(
        f"**Summary:** Total Builds: `{len(builds)}` | "
        f"Passed: `{len(success)}` | "
        f"Failed: `{len(failed)}` | "
        f"In-Progress: `{len(running)}`"
    )
    lines.append("")
    if not failed:
        lines.append("### Status: All completed builds passed!")
        if running:
            lines.append(f"> Note: {len(running)} builds are still running.")
        brief = (
            f"{cl_url} ({len(success)}/{len(builds)} passed, "
            f"0 failed, {len(running)} running)"
        )
        return "\n".join(lines), brief

    lines.append(f"## Failing Tryjobs ({len(failed)})")
    lines.append("")
    lines.append("| Builder | Status | Build ID | Summary |")
    lines.append("| :--- | :--- | :--- | :--- |")
    failed_names = {b.get("builder", {}).get("builder", "") for b in failed}
    brief_items = []
    for b in sorted(
        failed, key=lambda x: x.get("builder", {}).get("builder", "")
    ):
        bid = b.get("id", "unknown")
        b_name = b.get("builder", {}).get("builder", "unknown")
        st = b.get("status", "FAILURE")
        first_line = _first_meaningful_summary_line(
            b.get("summaryMarkdown", "")
        )
        lines.append(f"| **{b_name}** | `{st}` | `{bid}` | {first_line} |")
        if (
            b_name == "run-postsubmit-tryjobs"
            or f"{b_name}-subbuild" in failed_names
        ):
            continue
        display_name = b_name.removesuffix("-subbuild")
        brief_items.append(
            f"{display_name} ({first_line})" if first_line else display_name
        )
    if not brief_items:
        brief_items = [
            b.get("builder", {}).get("builder", "unknown") for b in failed
        ]
    shown = "; ".join(brief_items[:3])
    if len(brief_items) > 3:
        shown += f"; +{len(brief_items) - 3} more"
    brief = f"{cl_url} ({len(failed)}/{len(builds)} failed: {shown})"
    return "\n".join(lines), brief


REAL_FAILURE_PATTERNS = re.compile(
    r"(?:"
    r"sdk/lib/c|"
    r"third_party/llvm-libc|"
    r"third_party/scudo|"
    r"llvm-libc|"
    r"gwp[_-]asan|"
    r"\bscudo\b|"
    r"zircon/system/ulib/c|"
    r"zircon/third_party/ulib/musl|"
    r"zircon/system/utest/core|"
    r"libc[_-]unittests|"
    r"libc[_-]test|"
    r"c[_-]ulib[_-]test|"
    r"scudo[_-]unittests|"
    r"gwp[_-]asan[_-]test|"
    r"core[_-]tests|"
    r"userboot|"
    r"ld[_-]tests|"
    r"dl[_-]tests|"
    r"elfldltl|"
    r"\.ifs\b|"
    r"ifs_shared_library|"
    r"golden.*(?:sdk/lib/c|libc)|"
    r"undefined symbol|"
    r"duplicate symbol|"
    r"ld\.lld:\s*error:"
    r")",
    re.IGNORECASE,
)

UNRELATED_FLAKE_PATTERNS = re.compile(
    r"(?:"
    r"Fastboot_error|"
    r"Timeout_attempting_to_reach_target|"
    r"Target_specification_.*_was_not_found|"
    r"botanist_ERROR|"
    r"could_not_resolve_target_IP_address|"
    r"failed to establish ssh|"
    r"sys_config:\s*ERROR|"
    r"astro_[a-z]+_release_to_canary|"
    r"fx_script_tests\.sh|"
    r"compare built artifacts in gen/build/bazel|"
    r"This failure may be a false rejection|"
    r"bot_died|"
    r"timed out|"
    r"mypy_checked|"
    r"bazel_tests_utils\.py|"
    r"screen-capture|"
    r"obj/vendor/"
    r")",
    re.IGNORECASE,
)


def _extract_failure_signatures(builder: str, summary_md: str) -> list[str]:
    """Extract normalized failure signatures for tracking persistent unrelated failures across builds/rollers."""
    sigs: list[str] = []
    for m in re.finditer(
        r"fuchsia-pkg://[^/\s`]+/([^#\s`]+)(?:#meta/([^\s`]+))?",
        summary_md or "",
    ):
        pkg, comp = m.group(1), m.group(2)
        if comp and comp not in (
            "integration_test.cm",
            "test-root.cm",
            "default.cm",
        ):
            sigs.append(f"test:{comp}")
        else:
            sigs.append(f"test:{pkg}")

    for m_host in re.finditer(
        r"(?:host_x64|linux_x64|host_arm64)/[^\s`]+/([A-Za-z0-9_.-]+)",
        summary_md or "",
    ):
        sigs.append(f"test:{m_host.group(1)}")

    for m_task in re.finditer(
        r"failed to process test results for \d+ tasks?:\s*([^\s`\[]+)",
        summary_md or "",
        re.IGNORECASE,
    ):
        task_name = re.sub(r"-\(\d+\)$", "", m_task.group(1).strip())
        sigs.append(f"task:{task_name}")

    for tok in (
        "bazel_tests_utils.py",
        "mypy_checked",
        "Fastboot_error",
        "Timeout_attempting_to_reach_target",
        "botanist_ERROR",
        "sys_config",
        "compare built artifacts in gen/build/bazel",
    ):
        if tok.lower() in (summary_md or "").lower():
            sigs.append(f"step:{tok}")

    if not sigs:
        norm_builder = re.sub(r"-rust(?=-|$)", "", builder)
        sigs.append(f"builder:{norm_builder}")

    seen = set()
    unique_sigs = []
    for s in sigs:
        if s not in seen:
            seen.add(s)
            unique_sigs.append(s)
    return unique_sigs


def _extract_failure_keywords(signatures: list[str]) -> list[str]:
    """Extract search keywords from failure signatures to match against gardener reverts/fixes on main."""
    keywords: list[str] = []
    for sig in signatures:
        _, _, val = sig.partition(":")
        val = re.sub(r"\.(?:cm|py|sh|cc|rs)$", "", val)
        val = re.sub(
            r"[-_](?:integration[-_])?tests?$", "", val, flags=re.IGNORECASE
        )
        if len(val) >= 4 and not sig.startswith("builder:"):
            keywords.append(val.lower())
            if "-" in val:
                keywords.append(val.replace("-", "_").lower())
            elif "_" in val:
                keywords.append(val.replace("_", "-").lower())
    return sorted(set(keywords))


def _parse_iso_utc(ts_str: str | None) -> datetime.datetime | None:
    """Parse an ISO-8601 UTC timestamp string from Buildbucket or LUCI."""
    if not ts_str:
        return None
    try:
        if "." in ts_str:
            main_p, frac = ts_str.split(".", 1)
            frac_digits = frac.rstrip("Z")[:6]
            clean = f"{main_p}.{frac_digits}+00:00"
        else:
            clean = ts_str.rstrip("Z") + "+00:00"
        return datetime.datetime.fromisoformat(clean)
    except Exception:
        return None


def classify_roll_failures(
    roller_name: str,
    latest_build: dict,
    latest_steps: dict,
    cl_builds: list[dict],
) -> dict:
    """Classify a roller build's failures into unrelated flakes vs actionable issues."""
    failed = [
        b for b in cl_builds if b.get("status") in ("FAILURE", "INFRA_FAILURE")
    ]
    failed_names = {b.get("builder", {}).get("builder", "") for b in failed}
    leaf_failed = [
        b
        for b in sorted(
            failed, key=lambda x: x.get("builder", {}).get("builder", "")
        )
        if b.get("builder", {}).get("builder", "") != "run-postsubmit-tryjobs"
        and f"{b.get('builder', {}).get('builder', '')}-subbuild"
        not in failed_names
    ]

    actionable: list[dict] = []
    flakes: list[dict] = []
    flake_signatures: list[str] = []

    for b in leaf_failed:
        b_name = (
            b.get("builder", {})
            .get("builder", "unknown")
            .removesuffix("-subbuild")
        )
        summary = b.get("summaryMarkdown", "") or ""
        first_line = _first_meaningful_summary_line(summary)
        sigs = _extract_failure_signatures(b_name, summary)
        entry = {
            "build_id": b.get("id", "unknown"),
            "builder": b_name,
            "status": b.get("status", "FAILURE"),
            "first_line": first_line,
            "summary": summary,
            "signatures": sigs,
        }
        if REAL_FAILURE_PATTERNS.search(summary):
            actionable.append(entry)
        elif (
            b.get("status") == "INFRA_FAILURE"
            or UNRELATED_FLAKE_PATTERNS.search(summary)
            or re.search(r"^\d+\s+tests?\s+failed:", first_line, re.IGNORECASE)
        ):
            flakes.append(entry)
            for s in sigs:
                if s not in flake_signatures:
                    flake_signatures.append(s)
        else:
            actionable.append(entry)

    # Also detect when the roller build itself failed before/outside CQ tryjobs
    latest_status = latest_build.get("status", "UNKNOWN")
    if (
        latest_status in ("FAILURE", "INFRA_FAILURE")
        and not actionable
        and not flakes
    ):
        roller_summary = latest_build.get("summaryMarkdown", "") or ""
        first_line = _first_meaningful_summary_line(roller_summary)
        if "tryjobs failed" not in roller_summary.lower():
            actionable.append(
                {
                    "build_id": latest_build.get("id", "unknown"),
                    "builder": roller_name,
                    "status": latest_status,
                    "first_line": first_line or "roller recipe step failed",
                    "summary": roller_summary,
                    "signatures": [],
                }
            )

    cl_num = latest_steps.get("cl_num")
    roll_rev = (latest_steps.get("roll_revision") or "")[:12]
    issue_key = None
    issue_title = None
    if actionable:
        primary = actionable[0]
        safe_roller = (
            roller_name
            if re.fullmatch(r"[A-Za-z0-9_.-]+", str(roller_name))
            else "roller"
        )
        safe_builder = (
            primary["builder"]
            if re.fullmatch(r"[A-Za-z0-9_.-]+", str(primary.get("builder", "")))
            else "builder"
        )
        safe_cl = (
            str(cl_num)
            if cl_num and re.fullmatch(r"[0-9]+", str(cl_num))
            else None
        )
        safe_bid = (
            str(latest_build.get("id", ""))
            if re.fullmatch(r"[0-9]+", str(latest_build.get("id", "")))
            else "unknown"
        )
        safe_rev = roll_rev if re.fullmatch(r"[0-9a-f]{7,40}", roll_rev) else ""
        sig_token = "build_failure"
        for cand_sig in primary.get("signatures") or []:
            if re.fullmatch(r"[A-Za-z0-9_./:-]{1,64}", str(cand_sig)):
                sig_token = str(cand_sig)
                break
        target_label = f"fxr/{safe_cl}" if safe_cl else f"build {safe_bid}"
        rev_suffix = f" ({safe_rev})" if safe_rev else ""
        issue_title = (
            f"[{safe_roller}] {target_label}: "
            f"{safe_builder} ({sig_token}){rev_suffix}"
        )
        issue_key = (
            f"{safe_roller}:{safe_cl or safe_bid}:{safe_builder}:{sig_token}"
        )

    return {
        "actionable": actionable,
        "flakes": flakes,
        "flake_signatures": flake_signatures,
        "issue_key": issue_key,
        "issue_title": issue_title,
    }


def fetch_tree_status(
    fuchsia_root: Path, tree_name: str = "fuchsia-stem"
) -> dict:
    """Query LUCI TreeStatus for current state and recent closure/reopen history."""
    info = {
        "tree": tree_name,
        "general_state": "OPEN",
        "message": "",
        "create_time": "",
        "create_user": "",
        "last_reopened_at": None,
        "last_reopened_message": None,
        "last_closed_at": None,
        "last_closed_message": None,
    }
    try:
        token = get_luci_token(fuchsia_root)
        req = urllib.request.Request(
            "https://treestatus.api.luci.app/prpc/luci.tree_status.v1.TreeStatus/ListStatus",
            data=json.dumps(
                {"parent": f"trees/{tree_name}/status", "pageSize": 25}
            ).encode("utf-8"),
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
            method="POST",
        )
        raw = urllib.request.urlopen(req, timeout=15).read().decode("utf-8")
        if raw.startswith(")]}"):
            raw = raw.split("\n", 1)[1]
        statuses = json.loads(raw).get("status", [])
        if statuses:
            latest = statuses[0]
            info["general_state"] = latest.get("generalState", "OPEN")
            info["message"] = latest.get("message", "")
            info["create_time"] = latest.get("createTime", "")
            info["create_user"] = latest.get("createUser", "")
            for idx in range(len(statuses) - 1):
                curr = statuses[idx]
                older = statuses[idx + 1]
                if (
                    curr.get("generalState") == "OPEN"
                    and older.get("generalState") != "OPEN"
                    and not info["last_reopened_at"]
                ):
                    info["last_reopened_at"] = curr.get("createTime")
                    info["last_reopened_message"] = curr.get("message", "")
                    info["last_closed_at"] = older.get("createTime")
                    info["last_closed_message"] = older.get("message", "")
                    break
    except Exception:
        pass
    return info


def fetch_gardener_and_main_fixes(
    since_dt: datetime.datetime | None, keywords: list[str]
) -> dict:
    """Inspect recent commits on fuchsia.git main for gardener reverts or fixes matching failure keywords."""
    result: dict = {
        "reverts_since_roll": [],
        "matching_fixes_since_roll": [],
    }
    try:
        url = "https://fuchsia.googlesource.com/fuchsia/+log/refs/heads/main?n=40&format=JSON"
        raw = urllib.request.urlopen(url, timeout=15).read().decode("utf-8")
        if raw.startswith(")]}"):
            raw = raw.split("\n", 1)[1]
        commits = json.loads(raw).get("log", [])
        for c in commits:
            t_str = c.get("committer", {}).get("time", "")
            try:
                c_dt = datetime.datetime.strptime(
                    t_str, "%a %b %d %H:%M:%S %Y %z"
                ).astimezone(datetime.timezone.utc)
            except Exception:
                continue
            if since_dt and c_dt <= since_dt:
                continue
            msg = c.get("message", "")
            subj = msg.splitlines()[0] if msg else ""
            entry = {
                "short_sha": c.get("commit", "")[:12],
                "subject": subj,
                "time": c_dt.isoformat(),
            }
            is_revert = bool(
                re.search(r"^(?:Revert\b|\[revert\])", subj, re.IGNORECASE)
            )
            if is_revert:
                result["reverts_since_roll"].append(entry)
            msg_lower = msg.lower()
            if keywords and any(kw in msg_lower for kw in keywords):
                if is_revert or re.search(
                    r"\b(?:fix|disable|flake|flaky|skip|workaround|revert)\b",
                    msg_lower,
                ):
                    result["matching_fixes_since_roll"].append(entry)
    except Exception:
        pass
    return result


def check_ci_builders_for_flakes(
    fuchsia_root: Path,
    luci_project: str,
    flakes: list[dict],
    flake_signatures: list[str],
    since_dt: datetime.datetime | None,
) -> dict:
    """Check corresponding global.ci builders to see if the failure is also happening on CI or has recovered."""
    unique_builders = sorted(
        {f["builder"] for f in flakes if f.get("builder")}
    )[:4]
    sig_set = set(flake_signatures)
    now = datetime.datetime.now(datetime.timezone.utc)

    ci_streams: dict[str, dict] = {}
    ci_also_failed = False
    ci_still_failing = False
    ci_persistently_failing = False
    all_streams_green_since_roll = True
    all_streams_two_green_since_roll = True
    checked_any = False

    projects = [luci_project]
    if "fuchsia" not in projects:
        projects.append("fuchsia")

    for b_name in unique_builders:
        for proj in projects:
            ci_builder = f"{proj}/global.ci/{b_name}"
            try:
                builds = fetch_recent_builds(fuchsia_root, ci_builder, count=5)
            except Exception:
                continue
            if not builds:
                continue
            latest_dt = _parse_iso_utc(builds[0].get("createTime"))
            if latest_dt and (now - latest_dt).total_seconds() > 7 * 86400:
                continue

            completed = [
                b
                for b in builds
                if b.get("status") in ("SUCCESS", "FAILURE", "INFRA_FAILURE")
            ]
            if not completed:
                continue

            checked_any = True
            stream_failed_matching = False
            consec_matching_failures = 0
            counting_consec = True
            for b in completed:
                if b.get("status") in ("FAILURE", "INFRA_FAILURE"):
                    b_sigs = set(
                        _extract_failure_signatures(
                            b_name, b.get("summaryMarkdown", "")
                        )
                    )
                    if b_sigs & sig_set:
                        stream_failed_matching = True
                        ci_also_failed = True
                        if counting_consec:
                            consec_matching_failures += 1
                    else:
                        counting_consec = False
                else:
                    counting_consec = False

            latest_comp = completed[0]
            latest_comp_status = latest_comp.get("status")
            latest_comp_dt = _parse_iso_utc(latest_comp.get("createTime"))
            latest_comp_sigs = set(
                _extract_failure_signatures(
                    b_name, latest_comp.get("summaryMarkdown", "")
                )
            )
            stream_still_failing = latest_comp_status in (
                "FAILURE",
                "INFRA_FAILURE",
            ) and bool(latest_comp_sigs & sig_set)
            if stream_still_failing:
                ci_still_failing = True
            if consec_matching_failures >= 2:
                ci_persistently_failing = True

            green_since_roll = latest_comp_status == "SUCCESS" and (
                since_dt is None
                or (latest_comp_dt and latest_comp_dt > since_dt)
            )
            two_green_since_roll = (
                len(completed) >= 2
                and completed[0].get("status") == "SUCCESS"
                and completed[1].get("status") == "SUCCESS"
                and green_since_roll
            )
            if not green_since_roll:
                all_streams_green_since_roll = False
            if not two_green_since_roll:
                all_streams_two_green_since_roll = False

            ci_streams[ci_builder] = {
                "latest_completed_id": latest_comp.get("id"),
                "latest_completed_status": latest_comp_status,
                "latest_completed_create_time": latest_comp.get("createTime"),
                "latest_completed_summary": _first_meaningful_summary_line(
                    latest_comp.get("summaryMarkdown", "")
                ),
                "failed_with_same_signature": stream_failed_matching,
                "still_failing_with_same_signature": stream_still_failing,
                "consecutive_matching_failures": consec_matching_failures,
                "green_since_roll": green_since_roll,
                "two_green_since_roll": two_green_since_roll,
            }

    return {
        "streams": ci_streams,
        "ci_also_failed": ci_also_failed,
        "ci_still_failing": ci_still_failing,
        "ci_persistently_failing": ci_persistently_failing,
        "ci_green_after_roll": checked_any and all_streams_green_since_roll,
        "ci_two_green_after_roll": checked_any
        and all_streams_two_green_since_roll,
    }


# Backoff schedule (in seconds) by streak count for persistent unrelated failures:
# streak 1 (non-persistent): 0s; streak 2: 2h; streak 3: 4h; streak 4+: 8h.
PERSISTENT_BACKOFF_SECONDS = {
    1: 0,
    2: 2 * 3600,
    3: 4 * 3600,
}
MAX_PERSISTENT_BACKOFF_SECONDS = 8 * 3600


def evaluate_flake_retrigger_policy(
    fuchsia_root: Path,
    roller_name: str,
    roller_data: dict,
    all_reports: dict[str, dict],
    tree_info: dict,
    state: dict,
) -> dict:
    """Evaluate whether to retrigger immediately or back off due to a persistent unrelated failure."""
    builds = roller_data.get("recent_builds", [])
    latest = builds[0] if builds else {}
    latest_id = latest.get("id", "unknown")
    latest_status = latest.get("status", "UNKNOWN")
    classification = roller_data.get("failure_classification") or {}
    flakes = classification.get("flakes", [])
    current_sigs = set(classification.get("flake_signatures", []))

    # 1. Count consecutive runs of this roller with overlapping unrelated failure signatures
    same_roller_streak = 1 if current_sigs else 0
    for older in builds[1:]:
        older_sigs = set(older.get("flake_signatures", []))
        if older_sigs and (older_sigs & current_sigs):
            same_roller_streak += 1
        elif older.get("status") in ("SUCCESS", "FAILURE", "INFRA_FAILURE"):
            break

    # 2. Check if any other libc roller is currently hitting the same unrelated failure signature
    cross_roller_matches: list[str] = []
    for other_name, other_data in all_reports.items():
        if other_name == roller_name:
            continue
        other_builds = other_data.get("recent_builds", [])
        other_latest = other_builds[0] if other_builds else {}
        if other_latest.get("status") not in (
            "FAILURE",
            "INFRA_FAILURE",
            "STARTED",
        ):
            continue
        other_cls = other_data.get("failure_classification") or {}
        other_sigs = set(other_cls.get("flake_signatures", []))
        if current_sigs & other_sigs:
            cross_roller_matches.append(other_name)

    # 3. Check persisted streak state from monitor_state.json (only extending
    # beyond `same_roller_streak` when the consecutive streak fills the recent
    # builds window).
    roller_streak_state = state.get("flake_streaks", {}).get(roller_name, {})
    prev_sigs = set(roller_streak_state.get("signatures", []))
    prev_last_build = roller_streak_state.get("last_build_id")
    prev_roller_streak = roller_streak_state.get(
        "same_roller_streak", max(1, same_roller_streak)
    )
    if (
        current_sigs
        and (current_sigs & prev_sigs)
        and same_roller_streak >= max(2, len(builds))
    ):
        if prev_last_build and prev_last_build != latest_id:
            state_streak = max(prev_roller_streak + 1, same_roller_streak)
        else:
            state_streak = max(prev_roller_streak, same_roller_streak)
    else:
        state_streak = same_roller_streak

    since_dt = _parse_iso_utc(latest.get("createTime"))
    keywords = _extract_failure_keywords(sorted(current_sigs))
    gardener_info = fetch_gardener_and_main_fixes(since_dt, keywords)
    ci_info = check_ci_builders_for_flakes(
        fuchsia_root,
        roller_data.get("luci_project", "fuchsia"),
        flakes,
        sorted(current_sigs),
        since_dt,
    )

    is_persistent = (
        state_streak >= 2
        or bool(cross_roller_matches)
        or ci_info["ci_persistently_failing"]
    )
    streak_count = max(
        state_streak + (len(cross_roller_matches) if state_streak >= 2 else 0),
        2 if is_persistent else 1,
    )

    # 4. Evaluate recovery / gardener fix signals since the failing roll started
    fix_signals: list[str] = []
    for fix in gardener_info.get("matching_fixes_since_roll", []):
        fix_signals.append(
            f"matching fix/revert on main ({fix['short_sha']} {fix['subject']})"
        )

    reopened_dt = _parse_iso_utc(tree_info.get("last_reopened_at"))
    if (
        reopened_dt
        and since_dt
        and reopened_dt > since_dt
        and tree_info.get("general_state") == "OPEN"
        and not ci_info["ci_still_failing"]
    ):
        fix_signals.append(
            f"tree reopened after roll start ({tree_info['last_reopened_at']}: "
            f"{tree_info.get('last_reopened_message', '')})"
        )

    if not ci_info["ci_still_failing"]:
        if (streak_count <= 2 or not ci_info["ci_also_failed"]) and ci_info[
            "ci_green_after_roll"
        ]:
            fix_signals.append(
                "affected global.ci builder(s) passed on a newer build after roll start"
            )
        elif streak_count >= 3 and ci_info["ci_two_green_after_roll"]:
            fix_signals.append(
                "affected global.ci builder(s) passed 2 consecutive builds after roll start"
            )

    likely_fixed = bool(fix_signals)

    cooldown_sec = (
        0
        if not is_persistent
        else PERSISTENT_BACKOFF_SECONDS.get(
            streak_count, MAX_PERSISTENT_BACKOFF_SECONDS
        )
    )
    if (
        is_persistent
        and ci_info["ci_still_failing"]
        and not gardener_info.get("matching_fixes_since_roll")
    ):
        cooldown_sec = MAX_PERSISTENT_BACKOFF_SECONDS

    now = datetime.datetime.now(datetime.timezone.utc)
    ref_timestamps = [
        _parse_iso_utc(latest.get("endTime")),
        _parse_iso_utc(latest.get("createTime")),
        _parse_iso_utc(roller_streak_state.get("last_retrigger_at")),
    ]
    valid_refs = [dt for dt in ref_timestamps if dt is not None]
    last_attempt_dt = max(valid_refs) if valid_refs else now
    elapsed_sec = max(0, int((now - last_attempt_dt).total_seconds()))

    already_retriggered = (
        state.get("retriggered_builds", {}).get(roller_name) == latest_id
    )
    backoff_reasons: list[str] = []
    should_retrigger = True

    if already_retriggered:
        should_retrigger = False
        backoff_reasons.append(f"build {latest_id} already retriggered")
    if tree_info.get("general_state", "OPEN") != "OPEN":
        should_retrigger = False
        backoff_reasons.append(
            f"tree {tree_info.get('tree', 'fuchsia-stem')} is "
            f"{tree_info.get('general_state')} ({tree_info.get('message', '')})"
        )
    if is_persistent and latest_status == "STARTED":
        should_retrigger = False
        backoff_reasons.append(
            f"persistent failure (streak={streak_count}); waiting for STARTED build {latest_id} to finish"
        )
    if is_persistent and not likely_fixed and elapsed_sec < cooldown_sec:
        should_retrigger = False
        rem_min = max(1, (cooldown_sec - elapsed_sec) // 60)
        backoff_reasons.append(
            f"persistent failure backoff (streak={streak_count}, "
            f"elapsed={elapsed_sec // 60}m / cooldown={cooldown_sec // 60}m, "
            f"{rem_min}m remaining; no fix signal yet)"
        )
    if (
        is_persistent
        and ci_info["ci_still_failing"]
        and not gardener_info.get("matching_fixes_since_roll")
        and elapsed_sec < max(cooldown_sec, 2 * 3600)
    ):
        should_retrigger = False
        backoff_reasons.append(
            "corresponding global.ci builder latest completed build is still failing with the same signature"
        )

    return {
        "should_retrigger": should_retrigger,
        "already_retriggered": already_retriggered,
        "is_persistent": is_persistent,
        "same_roller_streak": state_streak,
        "streak_count": streak_count,
        "signatures": sorted(current_sigs),
        "cross_roller_matches": cross_roller_matches,
        "cooldown_seconds": cooldown_sec,
        "elapsed_seconds": elapsed_sec,
        "likely_fixed": likely_fixed,
        "fix_signals": fix_signals,
        "backoff_reasons": backoff_reasons,
        "tree": tree_info,
        "ci_health": ci_info,
        "gardener_activity": gardener_info,
    }


def prune_duplicate_baseline_conversations(
    worktree_dir: Path, state: dict
) -> str | None:
    """Ensure at most one baseline roller status conversation exists in worktree_dir."""
    ls_addr = os.environ.get("ANTIGRAVITY_LS_ADDRESS")
    csrf = os.environ.get("ANTIGRAVITY_CSRF_TOKEN")
    if not ls_addr or not csrf:
        return state.get("baseline_conversation_id")

    base_url = f"http://{ls_addr}/exa.language_server_pb.LanguageServerService"
    headers = {
        "Content-Type": "application/json",
        "x-codeium-csrf-token": csrf,
    }
    try:
        req = urllib.request.Request(
            f"{base_url}/GetAllCascadeTrajectories",
            data=b"{}",
            headers=headers,
            method="POST",
        )
        data = json.loads(
            urllib.request.urlopen(req, timeout=10).read().decode("utf-8")
        )
    except Exception:
        return state.get("baseline_conversation_id")

    wt_uri = f"file://{worktree_dir}"
    baseline_convs: list[tuple[str, str]] = []
    for cid, info in data.get("trajectorySummaries", {}).items():
        ws_uris = [
            w.get("workspaceFolderAbsoluteUri", "")
            for w in info.get("workspaces", [])
        ]
        if wt_uri not in ws_uris:
            continue
        summary = (info.get("summary") or "").strip()
        if summary.startswith("Fuchsia Libc Roller Status"):
            baseline_convs.append((info.get("createdTime", ""), cid))

    if not baseline_convs:
        return state.get("baseline_conversation_id")

    baseline_convs.sort()
    keep_cid = baseline_convs[-1][1]
    for _, old_cid in baseline_convs[:-1]:
        try:
            del_req = urllib.request.Request(
                f"{base_url}/DeleteCascadeTrajectory",
                data=json.dumps({"cascade_id": old_cid}).encode("utf-8"),
                headers=headers,
                method="POST",
            )
            urllib.request.urlopen(del_req, timeout=10).read()
            print(
                f"[MONITOR] Deleted duplicate baseline status conversation {old_cid}"
            )
        except Exception:
            pass

    state["baseline_conversation_id"] = keep_cid
    return keep_cid


def load_monitor_state(state_file: Path) -> dict:
    """Load persistent monitor state from disk."""
    state: dict = {
        "baseline_conversation_id": None,
        "retriggered_builds": {},
        "flake_streaks": {},
        "alerted_issues": {},
        "alerted_titles": {},
    }
    if state_file.exists():
        try:
            loaded = json.loads(state_file.read_text())
            if isinstance(loaded, dict):
                state.update(loaded)
        except Exception:
            pass
    return state


def save_monitor_state(state_file: Path, state: dict) -> None:
    """Write persistent monitor state to disk."""
    state_file.parent.mkdir(parents=True, exist_ok=True)
    state_file.write_text(json.dumps(state, indent=2) + "\n")


def run_monitor_actions(
    fuchsia_root: Path,
    worktree_dir: Path,
    rollers: dict[str, dict],
    report: dict[str, dict],
    state: dict,
    state_file: Path,
) -> None:
    """Auto-retrigger unrelated flakes (with persistent-failure backoff) and spawn issue-named conversations only for actionable failures."""
    prune_duplicate_baseline_conversations(worktree_dir, state)

    bb_bin = fuchsia_root / "prebuilt" / "tools" / "buildbucket" / "bb"
    rdb_bin = fuchsia_root / "prebuilt" / "tools" / "resultdb" / "rdb"
    now_iso = datetime.datetime.now(datetime.timezone.utc).isoformat()
    state_dirty = False

    for name, data in report.items():
        builds = data.get("recent_builds", [])
        if not builds:
            print(f"[MONITOR] {name}: no builds found")
            continue
        latest = builds[0]
        latest_id = latest.get("id", "unknown")
        latest_status = latest.get("status", "UNKNOWN")
        if latest_status == "SUCCESS":
            if name in state.get("flake_streaks", {}):
                state["flake_streaks"].pop(name, None)
                state_dirty = True
            print(
                f"[MONITOR] {name}: latest build {latest_id} succeeded (no conversation needed)"
            )
            continue

        classification = data.get("failure_classification") or {}
        actionable = classification.get("actionable", [])
        flakes = classification.get("flakes", [])

        if not actionable and not flakes:
            print(
                f"[MONITOR] {name}: build {latest_id} is {latest_status} with 0 failures so far (no conversation needed)"
            )
            continue

        if not actionable and flakes:
            policy = data.get("retrigger_policy") or {}
            flake_desc = "; ".join(
                f"{f['builder']} ({f['first_line']})" for f in flakes[:3]
            )
            streak_entry = state.setdefault("flake_streaks", {}).setdefault(
                name, {}
            )
            streak_entry["signatures"] = policy.get(
                "signatures", classification.get("flake_signatures", [])
            )
            streak_entry["same_roller_streak"] = policy.get(
                "same_roller_streak", 1
            )
            streak_entry["streak_count"] = policy.get("streak_count", 1)
            streak_entry["last_build_id"] = latest_id
            streak_entry["is_persistent"] = policy.get("is_persistent", False)
            state_dirty = True

            if policy.get("should_retrigger"):
                fix_note = (
                    f" (fix signals: {'; '.join(policy['fix_signals'])})"
                    if policy.get("fix_signals")
                    else ""
                )
                try:
                    resp = retrigger_roller(
                        fuchsia_root,
                        rollers,
                        name,
                        f"Auto-retrigger after unrelated Fuchsia flake on {latest_id}{fix_note}",
                    )
                    state.setdefault("retriggered_builds", {})[name] = latest_id
                    streak_entry["last_retrigger_at"] = now_iso
                    save_monitor_state(state_file, state)
                    state_dirty = False
                    print(
                        f"[MONITOR] {name}: auto-retriggered after unrelated flake(s) [{flake_desc}] "
                        f"on build {latest_id}{fix_note}: {json.dumps(resp)} (no conversation needed)"
                    )
                except Exception as exc:
                    print(
                        f"[MONITOR] {name}: failed to auto-retrigger build {latest_id}: {exc}"
                    )
            else:
                reasons = "; ".join(
                    policy.get("backoff_reasons") or ["holding off retrigger"]
                )
                print(
                    f"[MONITOR] {name}: holding off retrigger on build {latest_id} "
                    f"for unrelated flake(s) [{flake_desc}] -- {reasons} (no conversation needed)"
                )
            continue

        safe_name = (
            name if re.fullmatch(r"[A-Za-z0-9_.-]+", str(name)) else "roller"
        )
        safe_latest_id = (
            str(latest_id)
            if re.fullmatch(r"[0-9]+", str(latest_id))
            else "unknown"
        )
        safe_status = (
            str(latest_status)
            if re.fullmatch(r"[A-Z_]+", str(latest_status))
            else "UNKNOWN"
        )
        issue_key = (
            classification.get("issue_key") or f"{safe_name}:{safe_latest_id}"
        )
        issue_title = (
            classification.get("issue_title")
            or f"[{safe_name}] Issue on build {safe_latest_id}"
        )
        existing_conv = state.get("alerted_issues", {}).get(
            issue_key
        ) or state.get("alerted_titles", {}).get(issue_title)
        if existing_conv:
            print(
                f"[MONITOR] {safe_name}: actionable issue '{issue_title}' already alerted in "
                f"conversation {existing_conv}; skipping duplicate conversation"
            )
            continue

        raw_cl_url = str(data.get("active_or_failed_cl") or "")
        safe_cl_url = (
            raw_cl_url
            if re.fullmatch(
                r"https://fuchsia-review\.googlesource\.com/c/fuchsia/\+/[0-9]+(?:/[0-9]+)?",
                raw_cl_url,
            )
            else "N/A"
        )
        lf = data.get("last_failed_run") or {}
        lf_bid = str(lf.get("build_id") or "")
        safe_lf_url = (
            f"https://cr-buildbucket.appspot.com/build/{lf_bid}"
            if re.fullmatch(r"[0-9]+", lf_bid)
            else "none"
        )
        pending_commits = data.get("pending", {}).get("pending_commits", [])
        safe_shas = [
            str(c.get("short_sha", ""))
            for c in pending_commits[:15]
            if re.fullmatch(r"[0-9a-f]{7,40}", str(c.get("short_sha", "")))
        ]
        commits_lines = "\n".join(f"- `{sha}`" for sha in safe_shas)
        safe_failure_items = []
        for f in actionable[:10]:
            f_builder = str(f.get("builder", ""))
            f_bid = str(f.get("build_id", ""))
            if re.fullmatch(r"[A-Za-z0-9_.-]+", f_builder) and re.fullmatch(
                r"[0-9]+", f_bid
            ):
                safe_failure_items.append(
                    f"- **{f_builder}** (`https://cr-buildbucket.appspot.com/build/{f_bid}`)"
                )
        failures_lines = "\n".join(safe_failure_items) or "- See roller build"

        # Security note: Only strictly validated identifiers/IDs (never untrusted
        # commit messages or build failure logs) are interpolated into the prompt.
        prompt = (
            f"Investigate an actionable roller failure on `{safe_name}` that needs attention:\n\n"
            f"- **Issue**: {issue_title}\n"
            f"- **Roller Build**: https://cr-buildbucket.appspot.com/build/{safe_latest_id} (`{safe_status}`)\n"
            f"- **Roll Gerrit CL**: {safe_cl_url}\n"
            f"- **Last Failed Build**: {safe_lf_url}\n\n"
            f"### Failing Builder(s)\n{failures_lines}\n\n"
            f"### Pending Upstream Commit SHAs ({len(safe_shas)})\n{commits_lines or '- None'}\n\n"
            "**CRITICAL WORKSPACE & WORKTREE RULE**:\n"
            f"- NEVER modify `{fuchsia_root}` (keep it as a clean reference checkout).\n"
            f"- Use the dedicated Jiri worktree at `{worktree_dir}` for any local inspection, "
            "reproduction, or fixes.\n\n"
            "### Next Steps\n"
            f"1. Inspect the failing step/test logs using `{bb_bin} log` and `{rdb_bin} query`.\n"
            "2. Identify the root cause and culprit upstream commit(s), and summarize what needs "
            "the user's attention (including the roll status, whether it restored a previous roll "
            "attempt or added new commits, and the last failed run summary + Gerrit link)."
        )

        res = subprocess.run(
            [
                "agentapi",
                "new-conversation",
                f"--title={issue_title}",
                "--",
                prompt,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        conv_id = "spawned"
        if res.returncode == 0:
            m_cid = re.search(r'"conversationId":\s*"([^"]+)"', res.stdout)
            if m_cid:
                conv_id = m_cid.group(1)
            state.setdefault("alerted_issues", {})[issue_key] = conv_id
            state.setdefault("alerted_titles", {})[issue_title] = conv_id
            save_monitor_state(state_file, state)
            state_dirty = False
            print(
                f"[MONITOR] {name}: spawned conversation {conv_id} titled '{issue_title}'"
            )
        else:
            print(
                f"[MONITOR] {name}: agentapi new-conversation failed ({res.returncode}): {res.stderr.strip()}"
            )

    if state_dirty:
        save_monitor_state(state_file, state)


def retrigger_roller(
    fuchsia_root: Path,
    rollers: dict[str, dict],
    roller_name: str,
    reason: str,
) -> dict:
    """Emit a Buildbucket trigger to LUCI Scheduler for the specified roller."""
    if roller_name not in rollers:
        raise ValueError(
            f"Unknown roller '{roller_name}'. Discovered rollers: {list(rollers)}"
        )
    info = rollers[roller_name]
    if not re.search(r"-[0-9a-f]{6,}$", info["scheduler_job"]):
        update_scheduler_job_from_builds(
            info, fetch_recent_builds(fuchsia_root, info["builder"], count=3)
        )
    token = get_luci_token(fuchsia_root)
    trigger_id = f"manual-{uuid.uuid4()}"
    payload = {
        "batches": [
            {
                "jobs": [
                    {
                        "project": info["luci_project"],
                        "job": info["scheduler_job"],
                    }
                ],
                "trigger": {
                    "id": trigger_id,
                    "title": reason,
                    "buildbucket": {},
                },
            }
        ]
    }
    return scheduler_rpc(token, "EmitTriggers", payload)


def _validate_roller_config_value(key: str, val: str) -> str:
    """Validate discovered roller metadata before writing to SKILL.md or DEFAULT_ROLLERS."""
    patterns = {
        "name": r"[A-Za-z0-9_.-]{1,64}",
        "bucket": r"[A-Za-z0-9_.-]{1,64}",
        "manifest_project": r"[A-Za-z0-9_./-]{1,128}",
        "remote": r"https://[A-Za-z0-9_./-]+",
        "path": r"[A-Za-z0-9_./-]{1,128}",
        "schedule": r"[A-Za-z0-9_.* /:-]{1,64}",
    }
    pat = patterns.get(key, r"[A-Za-z0-9_./:-]{1,128}")
    if not re.fullmatch(pat, val):
        raise ValueError(f"Refusing to write invalid {key} value: {val!r}")
    return val


def render_skill_table(rollers: dict[str, dict]) -> str:
    """Render the Markdown table of target rollers for SKILL.md (without volatile hex job hashes)."""
    lines = [
        "<!-- BEGIN AUTO-GENERATED ROLLERS TABLE -->",
        "| Roller Builder | LUCI Bucket | Upstream Mirror | Target Path in `fuchsia.git` | Schedule |",
        "| :--- | :--- | :--- | :--- | :--- |",
    ]
    for name, info in rollers.items():
        s_name = _validate_roller_config_value("name", str(name))
        s_bucket = _validate_roller_config_value("bucket", str(info["bucket"]))
        s_remote = _validate_roller_config_value("remote", str(info["remote"]))
        s_path = _validate_roller_config_value("path", str(info["path"]))
        s_sched = _validate_roller_config_value(
            "schedule", str(info["schedule"])
        )
        lines.append(
            f"| `{s_name}` | `{s_bucket}` | "
            f"`{s_remote}` | `{s_path}` | `{s_sched}` |"
        )
    lines.append("<!-- END AUTO-GENERATED ROLLERS TABLE -->")
    return "\n".join(lines)


def render_default_rollers_block(rollers: dict[str, dict]) -> str:
    """Render the DEFAULT_ROLLERS Python dict block for check_libc_rollers.py (without hex job hashes)."""
    lines = [
        "# BEGIN AUTO-GENERATED DEFAULT_ROLLERS",
        "DEFAULT_ROLLERS = {",
    ]
    for name, info in rollers.items():
        s_name = _validate_roller_config_value("name", str(name))
        lines.append(f"    {json.dumps(s_name)}: {{")
        for k in (
            "bucket",
            "manifest_project",
            "remote",
            "path",
            "schedule",
        ):
            s_val = _validate_roller_config_value(k, str(info[k]))
            lines.append(f"        {json.dumps(k)}: {json.dumps(s_val)},")
        lines.append("    },")
    lines.append("}")
    lines.append("# END AUTO-GENERATED DEFAULT_ROLLERS")
    return "\n".join(lines)


def _replace_delimited_section(
    text: str, begin_marker: str, end_marker: str, replacement: str
) -> str:
    """Replace a marker-delimited section using string slicing (never re.sub)."""
    start_idx = text.find(begin_marker)
    if start_idx == -1:
        return text
    end_idx = text.find(end_marker, start_idx)
    if end_idx == -1:
        return text
    end_idx += len(end_marker)
    return text[:start_idx] + replacement + text[end_idx:]


def sync_skill_files(
    rollers: dict[str, dict], force_write: bool = False
) -> list[str]:
    """Check if SKILL.md or DEFAULT_ROLLERS drifted from discovered LUCI config and self-update."""
    updates = []
    script_path = Path(__file__).resolve()
    skill_md_path = script_path.parent.parent / "SKILL.md"
    if skill_md_path.exists():
        old_text = skill_md_path.read_text()
        new_table = render_skill_table(rollers)
        new_text = _replace_delimited_section(
            old_text,
            "<!-- BEGIN AUTO-GENERATED ROLLERS TABLE -->",
            "<!-- END AUTO-GENERATED ROLLERS TABLE -->",
            new_table,
        )
        if new_text != old_text:
            if force_write and os.access(skill_md_path, os.W_OK):
                skill_md_path.write_text(new_text)
                updates.append(f"Self-updated LUCI table in {skill_md_path}")
            else:
                updates.append(
                    f"Detected LUCI config drift in {skill_md_path} (run with --update-skill to write)"
                )

    if script_path.exists():
        old_py = script_path.read_text()
        new_block = render_default_rollers_block(rollers)
        new_py = _replace_delimited_section(
            old_py,
            "# BEGIN AUTO-GENERATED DEFAULT_ROLLERS",
            "# END AUTO-GENERATED DEFAULT_ROLLERS",
            new_block,
        )
        if new_py != old_py:
            if force_write and os.access(script_path, os.W_OK):
                script_path.write_text(new_py)
                updates.append(f"Self-updated DEFAULT_ROLLERS in {script_path}")
            else:
                updates.append(
                    f"Detected LUCI config drift in {script_path} (run with --update-skill to write)"
                )
    return updates


UI_BIND_HOST = "127.0.0.1"
PRELOAD_JS_TEXT = """\
(function() {
  const params = new URLSearchParams(window.location.search);
  const token = params.get('token');
  const conversationId = params.get('conversationId');
  window.addEventListener('message', (event) => {
    if (event.data && event.data.type === 'theme-change' && event.data.payload && event.data.payload.cssVariables) {
      for (const [k, v] of Object.entries(event.data.payload.cssVariables)) {
        document.documentElement.style.setProperty(k, v);
      }
    }
  });
  function secureFetch(url, options = {}) {
    const headers = new Headers(options.headers || {});
    headers.set('Content-Type', 'application/json');
    if (token) headers.set('X-Sidecar-Token', token);
    return fetch(url, { ...options, headers });
  }
  window.sidecar = { conversationId, fetch: secureFetch };
})();
"""


def _iso_to_epoch(ts: str | None) -> float | None:
    """Convert an ISO-8601 timestamp string to UTC epoch seconds."""
    dt = _parse_iso_utc(ts or "")
    return dt.timestamp() if dt else None


def _gerrit_date_to_epoch(ts: str | None) -> float | None:
    """Convert a Gerrit timestamp 'YYYY-MM-DD HH:MM:SS.000000000' (UTC) to epoch."""
    if not ts:
        return None
    s = ts.strip()
    if "." in s:
        s = s.split(".", 1)[0]
    try:
        dt = datetime.datetime.strptime(s, "%Y-%m-%d %H:%M:%S").replace(
            tzinfo=datetime.timezone.utc
        )
        return dt.timestamp()
    except ValueError:
        return _iso_to_epoch(ts)


def format_approx_local_time(
    epoch_sec: float | None, now_sec: float | None = None
) -> str:
    """Format an epoch timestamp as an approximate local time (e.g., '~1:40pm', 'Jul 14')."""
    if not epoch_sec:
        return ""
    now = now_sec if now_sec is not None else time.time()
    diff_sec = max(0.0, now - epoch_sec)
    local_dt = datetime.datetime.fromtimestamp(epoch_sec).astimezone()
    if diff_sec > 3 * 86400:
        return f"{local_dt.strftime('%b')} {local_dt.day}"
    rounded_ts = round(epoch_sec / 300.0) * 300.0
    r_dt = datetime.datetime.fromtimestamp(rounded_ts).astimezone()
    hour = r_dt.hour % 12 or 12
    ampm = "pm" if r_dt.hour >= 12 else "am"
    if r_dt.minute == 0:
        return f"~{hour}{ampm}"
    return f"~{hour}:{r_dt.minute:02d}{ampm}"


def format_relative_ago(
    epoch_sec: float | None, now_sec: float | None = None
) -> str:
    """Format an epoch timestamp in relative terms ('just now', '~35m ago', '~1.5h ago')."""
    if not epoch_sec:
        return ""
    now = now_sec if now_sec is not None else time.time()
    diff_sec = max(0.0, now - epoch_sec)
    if diff_sec < 90:
        return "just now"
    mins = round(diff_sec / 60.0)
    if mins < 60:
        r_mins = round(mins / 5) * 5 if mins >= 10 else mins
        return f"~{r_mins}m ago"
    hours = diff_sec / 3600.0
    if hours < 36:
        if hours < 4:
            r_hours = round(hours * 2) / 2
            h_str = (
                f"{int(r_hours)}"
                if r_hours == int(r_hours)
                else f"{r_hours:.1f}"
            )
            return f"~{h_str}h ago"
        return f"~{round(hours)}h ago"
    days = diff_sec / 86400.0
    if days < 30:
        return f"~{round(days)}d ago"
    months = round((days / 30.44) * 2) / 2
    m_str = f"{int(months)}" if months == int(months) else f"{months:.1f}"
    return f"~{m_str}mo ago"


def format_time_pair(
    epoch_sec: float | None, now_sec: float | None = None
) -> str:
    """Format an epoch timestamp as '<relative> (<approx local>)'."""
    if not epoch_sec:
        return ""
    rel = format_relative_ago(epoch_sec, now_sec)
    loc = format_approx_local_time(epoch_sec, now_sec)
    return f"{rel} ({loc})" if loc else rel


def fetch_last_landed_roll(
    fuchsia_root: Path,
    roller_name: str,
    manifest_project: str,
    pinned_rev: str,
    cache: dict[tuple[str, str], dict] | None = None,
) -> dict:
    """Resolve when the current pinned revision landed in fuchsia.git."""
    cache_key = (roller_name, pinned_rev)
    if cache is not None and cache_key in cache:
        return cache[cache_key]

    try:
        query = quote(
            f'project:fuchsia status:merged message:"[roll] Roll {manifest_project}"'
        )
        url = (
            "https://fuchsia-review.googlesource.com/changes/"
            f"?q={query}&n=1&o=CURRENT_REVISION"
        )
        req = urllib.request.Request(
            url, headers={"Accept": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            raw = resp.read().decode("utf-8")
        if raw.startswith(")]}'"):
            raw = raw.split("\n", 1)[1]
        data = json.loads(raw)
        if data:
            entry = data[0]
            cl_num = str(entry.get("_number", "")) or None
            epoch = _gerrit_date_to_epoch(
                entry.get("submitted") or entry.get("updated")
            )
            info = {
                "commit": (entry.get("current_revision") or "")[:12] or None,
                "epoch": epoch,
                "subject": entry.get("subject", ""),
                "cl_num": cl_num,
                "cl_url": (
                    f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{cl_num}"
                    if cl_num
                    else None
                ),
            }
            if cache is not None:
                cache[cache_key] = info
            return info
    except Exception:
        pass

    try:
        res = subprocess.run(
            [
                "git",
                "-C",
                str(fuchsia_root),
                "log",
                "-n",
                "1",
                "--format=%H|%ct|%B",
                "-S",
                pinned_rev[:12],
                "--",
                "manifests/third_party/all",
            ],
            capture_output=True,
            text=True,
            check=False,
            timeout=15,
        )
        out = res.stdout.strip()
        if out and "|" in out:
            parts = out.split("|", 2)
            commit_sha = parts[0][:12]
            epoch = float(parts[1]) if parts[1].isdigit() else None
            body = parts[2] if len(parts) > 2 else ""
            first_line = body.splitlines()[0] if body else ""
            cl_match = re.search(
                r"Reviewed-on:\s*https://fuchsia-review\.googlesource\.com/c/fuchsia/\+/(\d+)",
                body,
            )
            cl_num = cl_match.group(1) if cl_match else None
            info = {
                "commit": commit_sha,
                "epoch": epoch,
                "subject": first_line,
                "cl_num": cl_num,
                "cl_url": (
                    f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{cl_num}"
                    if cl_num
                    else None
                ),
            }
            if cache is not None:
                cache[cache_key] = info
            return info
    except Exception:
        pass

    return {
        "commit": None,
        "epoch": None,
        "subject": None,
        "cl_num": None,
        "cl_url": None,
    }


def fetch_watched_cl_status(fuchsia_root: Path, cl_num: str) -> dict | None:
    """Fetch live Gerrit and CQ status for an optionally watched CL."""
    if not re.fullmatch(r"[0-9]+", str(cl_num)):
        return None
    url = (
        f"https://fuchsia-review.googlesource.com/changes/{cl_num}"
        "?o=CURRENT_REVISION&o=LABELS&o=DETAILED_ACCOUNTS"
    )
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            raw = resp.read().decode("utf-8")
        if raw.startswith(")]}'"):
            raw = raw.split("\n", 1)[1]
        data = json.loads(raw)
    except Exception:
        return None

    status = data.get("status", "UNKNOWN")
    ps_num = data.get("current_revision_number")
    curr_rev = data.get("current_revision", "")
    rev_info = data.get("revisions", {}).get(curr_rev, {})
    ps_created = _gerrit_date_to_epoch(
        rev_info.get("created") or data.get("updated")
    )
    updated_epoch = _gerrit_date_to_epoch(data.get("updated"))

    labels_raw = data.get("labels", {})
    labels_summary = {}
    for lname in (
        "Code-Review",
        "Presubmit-Verified",
        "Commit-Queue",
        "Fuchsia-Auto-Submit",
    ):
        lobj = labels_raw.get(lname, {})
        val = 0
        if "approved" in lobj:
            val = 2
        elif "recommended" in lobj:
            val = 1
        elif "rejected" in lobj:
            val = -2
        elif "disliked" in lobj:
            val = -1
        elif "value" in lobj:
            val = lobj.get("value") or 0
        labels_summary[lname] = val

    cq_counts = None
    if ps_num and status == "NEW":
        try:
            _, builds, cl_err = fetch_cl_builds(
                fuchsia_root, str(cl_num), str(ps_num)
            )
            if cl_err is None and builds:
                total = len(builds)
                passed = sum(1 for b in builds if b.get("status") == "SUCCESS")
                failed = sum(
                    1
                    for b in builds
                    if b.get("status")
                    in ("FAILURE", "INFRA_FAILURE", "CANCELED")
                )
                running = sum(
                    1
                    for b in builds
                    if b.get("status") in ("STARTED", "SCHEDULED")
                )
                cq_counts = {
                    "total": total,
                    "passed": passed,
                    "failed": failed,
                    "running": running,
                }
        except Exception:
            pass

    return {
        "cl_num": str(cl_num),
        "patchset": ps_num,
        "cl_url": (
            f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{cl_num}/{ps_num}"
            if ps_num
            else f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{cl_num}"
        ),
        "subject": data.get("subject", f"fxr/{cl_num}"),
        "status": status,
        "ps_created_epoch": ps_created,
        "updated_epoch": updated_epoch,
        "unresolved_comment_count": data.get("unresolved_comment_count", 0),
        "total_comment_count": data.get("total_comment_count", 0),
        "labels": labels_summary,
        "cq": cq_counts,
    }


def _summarize_cq_builds(
    fuchsia_root: Path,
    cl_num: str,
    patchset: str | None,
    builds: list[dict],
) -> dict:
    """Summarize passing, failing, and running tryjobs for a roll CL."""
    total = len(builds)
    passed = sum(1 for b in builds if b.get("status") == "SUCCESS")
    failed_builds = [
        b
        for b in builds
        if b.get("status") in ("FAILURE", "INFRA_FAILURE", "CANCELED")
    ]
    running = sum(
        1 for b in builds if b.get("status") in ("STARTED", "SCHEDULED")
    )
    failure_summary = None
    if failed_builds:
        _, failure_summary = run_triage_cl(
            fuchsia_root, cl_num, patchset, builds=builds
        )
    return {
        "total": total,
        "passed": passed,
        "failed": len(failed_builds),
        "running": running,
        "failure_summary": failure_summary,
    }


DEFAULT_SLO_CONFIG: dict[str, dict] = {
    "llvm-libc-roller": {
        "target_hours": 12.0,
        "breach_hours": 24.0,
        "target_compliance_pct": 80.0,
        "cadence_note": "4h interval + Gitiles trigger (1-2 attempts <= 12h, breach > 24h)",
    },
    "scudo-roller": {
        "target_hours": 18.0,
        "breach_hours": 36.0,
        "target_compliance_pct": 80.0,
        "cadence_note": "Gitiles trigger + daily 05:00 UTC fallback (1-2 attempts <= 18h, breach > 36h)",
    },
    "gwp_asan-roller": {
        "target_hours": 18.0,
        "breach_hours": 36.0,
        "target_compliance_pct": 80.0,
        "cadence_note": "Gitiles trigger + daily 04:00 UTC fallback (1-2 attempts <= 18h, breach > 36h)",
    },
}


def _default_roller_slo_cfg(roller_name: str, schedule: str = "") -> dict:
    """Return default SLO thresholds for a roller."""
    if roller_name in DEFAULT_SLO_CONFIG:
        return dict(DEFAULT_SLO_CONFIG[roller_name])
    is_frequent = "interval" in schedule
    return {
        "target_hours": 12.0 if is_frequent else 18.0,
        "breach_hours": 24.0 if is_frequent else 36.0,
        "target_compliance_pct": 80.0,
        "cadence_note": f"schedule: {schedule or 'triggered'}",
    }


def fetch_recent_merged_rolls(
    manifest_project: str, limit: int = 15
) -> list[dict]:
    """Fetch recent merged roll CLs from Gerrit with convergence latency and attempt counts."""
    rolls: list[dict] = []
    try:
        query = quote(
            f'project:fuchsia status:merged message:"[roll] Roll {manifest_project}"'
        )
        url = (
            "https://fuchsia-review.googlesource.com/changes/"
            f"?q={query}&n={limit}&o=CURRENT_REVISION"
        )
        req = urllib.request.Request(
            url, headers={"Accept": "application/json"}
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            raw = resp.read().decode("utf-8")
        if raw.startswith(")]}'"):
            raw = raw.split("\n", 1)[1]
        entries = json.loads(raw)
        for entry in entries:
            subj = entry.get("subject", "")
            if re.search(r"^(?:Revert\b|\[revert\])", subj, re.IGNORECASE):
                continue
            cl_num = str(entry.get("_number", ""))
            created_ep = _gerrit_date_to_epoch(entry.get("created"))
            submitted_ep = _gerrit_date_to_epoch(
                entry.get("submitted") or entry.get("updated")
            )
            if not cl_num or not created_ep or not submitted_ep:
                continue
            conv_h = round(max(0.1, (submitted_ep - created_ep) / 3600.0), 2)
            # Gerrit creates a final rebase patchset on submit, so PS2 = 1 attempt, PS3 = 2 attempts
            rev_num = int(entry.get("current_revision_number") or 2)
            attempts = max(1, rev_num - 1)
            rolls.append(
                {
                    "cl_num": cl_num,
                    "commit": (entry.get("current_revision") or "")[:12],
                    "subject": subj,
                    "created_epoch": created_ep,
                    "submitted_epoch": submitted_ep,
                    "convergence_hours": conv_h,
                    "attempts": attempts,
                }
            )
    except Exception:
        pass
    return rolls


def _percentile(sorted_vals: list[float], pct: float) -> float:
    """Compute a linear-interpolated percentile (0..100) from a non-empty sorted list."""
    if not sorted_vals:
        return 0.0
    if len(sorted_vals) == 1:
        return round(sorted_vals[0], 2)
    idx = (pct / 100.0) * (len(sorted_vals) - 1)
    lo = int(idx)
    hi = min(lo + 1, len(sorted_vals) - 1)
    frac = idx - lo
    return round(sorted_vals[lo] * (1.0 - frac) + sorted_vals[hi] * frac, 2)


def load_health_history(history_file: Path) -> dict:
    """Load persistent roller health telemetry and SLO configuration from disk."""
    history: dict = {
        "schema_version": 1,
        "updated_at": None,
        "slo_config": {k: dict(v) for k, v in DEFAULT_SLO_CONFIG.items()},
        "rollers": {},
    }
    if history_file.exists():
        try:
            loaded = json.loads(history_file.read_text(encoding="utf-8"))
            if isinstance(loaded, dict):
                saved_cfg = loaded.get("slo_config")
                if isinstance(saved_cfg, dict):
                    for r_name, r_cfg in saved_cfg.items():
                        if isinstance(r_cfg, dict):
                            history["slo_config"].setdefault(
                                r_name, _default_roller_slo_cfg(r_name)
                            ).update(r_cfg)
                if isinstance(loaded.get("rollers"), dict):
                    history["rollers"] = loaded["rollers"]
        except Exception:
            pass
    return history


def save_health_history(history_file: Path, history: dict) -> None:
    """Persist roller health telemetry and SLO configuration to disk."""
    history["updated_at"] = datetime.datetime.now(
        datetime.timezone.utc
    ).isoformat()
    history_file.parent.mkdir(parents=True, exist_ok=True)
    history_file.write_text(
        json.dumps(history, indent=2) + "\n", encoding="utf-8"
    )


def apply_slo_overrides(history_file: Path, overrides: list[str]) -> dict:
    """Update per-roller SLO target/breach hours in roller_health_history.json (--set-slo)."""
    history = load_health_history(history_file)
    for spec in overrides:
        m = re.fullmatch(
            r"([A-Za-z0-9_.-]+)=([0-9]+(?:\.[0-9]+)?):([0-9]+(?:\.[0-9]+)?)",
            spec.strip(),
        )
        if not m:
            raise ValueError(
                f"Invalid --set-slo spec {spec!r}; expected <roller>=<target_hours>:<breach_hours> "
                "(e.g. llvm-libc-roller=12:24)"
            )
        r_name, target_s, breach_s = m.group(1), m.group(2), m.group(3)
        target_h = float(target_s)
        breach_h = float(breach_s)
        if target_h <= 0 or breach_h <= target_h:
            raise ValueError(
                f"Invalid SLO thresholds for {r_name}: target_hours ({target_h}) must be > 0 and < breach_hours ({breach_h})"
            )
        cfg = history["slo_config"].setdefault(
            r_name, _default_roller_slo_cfg(r_name)
        )
        cfg["target_hours"] = target_h
        cfg["breach_hours"] = breach_h
        cfg["updated_at"] = datetime.datetime.now(
            datetime.timezone.utc
        ).isoformat()
    save_health_history(history_file, history)
    return history["slo_config"]


def update_and_evaluate_roller_slo(
    history: dict,
    roller_name: str,
    manifest_project: str,
    schedule: str,
    p_info: dict,
    last_landed: dict,
    recent_builds: list[dict],
    failure_classification: dict | None,
    pending_attempt_ui: dict | None,
    backoff_ui: dict | None,
    now_epoch: float,
) -> dict:
    """Update historical roll/build stats for a roller and evaluate its current SLO health tier."""
    slo_cfg = history.setdefault("slo_config", {}).setdefault(
        roller_name, _default_roller_slo_cfg(roller_name, schedule)
    )
    target_h = float(slo_cfg.get("target_hours", 12.0))
    breach_h = float(slo_cfg.get("breach_hours", 24.0))
    target_comp_pct = float(slo_cfg.get("target_compliance_pct", 80.0))

    r_hist = history.setdefault("rollers", {}).setdefault(
        roller_name,
        {
            "last_gerrit_backfill_epoch": 0.0,
            "merged_rolls": [],
            "builds": [],
            "stats": {},
        },
    )

    existing_rolls: dict[str, dict] = {
        str(r["cl_num"]): r
        for r in r_hist.get("merged_rolls", [])
        if isinstance(r, dict) and r.get("cl_num")
    }
    last_landed_cl = str(last_landed.get("cl_num") or "")
    last_backfill = float(r_hist.get("last_gerrit_backfill_epoch") or 0.0)
    if (
        not existing_rolls
        or (last_landed_cl and last_landed_cl not in existing_rolls)
        or (now_epoch - last_backfill >= 1800.0)
    ):
        fetched_rolls = fetch_recent_merged_rolls(manifest_project, limit=15)
        if fetched_rolls:
            for fr in fetched_rolls:
                existing_rolls[str(fr["cl_num"])] = fr
            r_hist["last_gerrit_backfill_epoch"] = now_epoch

    merged_list = sorted(
        existing_rolls.values(),
        key=lambda x: float(x.get("submitted_epoch") or 0.0),
        reverse=True,
    )[:50]
    r_hist["merged_rolls"] = merged_list

    existing_builds: dict[str, dict] = {
        str(b["build_id"]): b
        for b in r_hist.get("builds", [])
        if isinstance(b, dict) and b.get("build_id")
    }
    for b in recent_builds:
        bid = str(b.get("id") or "")
        st = b.get("status", "")
        if not bid or st not in ("SUCCESS", "FAILURE", "INFRA_FAILURE"):
            continue
        c_ep = _iso_to_epoch(b.get("createTime"))
        e_ep = _iso_to_epoch(b.get("endTime") or b.get("createTime"))
        f_kind = "none"
        if st in ("FAILURE", "INFRA_FAILURE"):
            f_kind = "flake" if b.get("flake_signatures") else "failure"
        existing_builds[bid] = {
            "build_id": bid,
            "status": st,
            "create_epoch": c_ep,
            "end_epoch": e_ep,
            "attempt_kind": b.get("attempt_kind", ""),
            "failure_kind": f_kind,
        }
    builds_list = sorted(
        existing_builds.values(),
        key=lambda x: float(x.get("create_epoch") or 0.0),
        reverse=True,
    )[:100]
    r_hist["builds"] = builds_list

    # Compute empirical convergence & build success statistics
    conv_vals = sorted(
        float(r["convergence_hours"])
        for r in merged_list
        if r.get("convergence_hours") is not None
    )
    sample_rolls = len(conv_vals)
    p50_h = _percentile(conv_vals, 50.0)
    p90_h = _percentile(conv_vals, 90.0)
    max_h = round(conv_vals[-1], 2) if conv_vals else 0.0
    within_target_pct = (
        round(
            100.0 * sum(1 for v in conv_vals if v <= target_h) / sample_rolls,
            1,
        )
        if sample_rolls
        else 100.0
    )
    within_breach_pct = (
        round(
            100.0 * sum(1 for v in conv_vals if v <= breach_h) / sample_rolls,
            1,
        )
        if sample_rolls
        else 100.0
    )
    first_try_pct = (
        round(
            100.0
            * sum(1 for r in merged_list if int(r.get("attempts", 1)) <= 1)
            / sample_rolls,
            1,
        )
        if sample_rolls
        else 100.0
    )
    avg_attempts = (
        round(
            sum(int(r.get("attempts", 1)) for r in merged_list) / sample_rolls,
            2,
        )
        if sample_rolls
        else 1.0
    )
    sample_builds = len(builds_list)
    build_success_pct = (
        round(
            100.0
            * sum(1 for b in builds_list if b.get("status") == "SUCCESS")
            / sample_builds,
            1,
        )
        if sample_builds
        else 100.0
    )

    calibration_advice = "Collecting baseline roll history (<5 rolls)."
    if sample_rolls >= 5:
        suggested_target = max(4.0, round(p90_h * 2.0) / 2.0)
        if within_target_pct < target_comp_pct and avg_attempts >= 1.8:
            calibration_advice = (
                f"{within_target_pct:.0f}% <= {target_h:g}h target (goal >={target_comp_pct:.0f}%) "
                f"due to multi-attempt CQ flake retries (avg {avg_attempts:.1f} tries, P90 {p90_h:.1f}h). "
                f"Keep {target_h:g}h/{breach_h:g}h to highlight flake lag, or widen with "
                f"--set-slo {roller_name}={suggested_target:g}:{max(breach_h, round(suggested_target * 2)):g}."
            )
        elif within_target_pct < target_comp_pct:
            calibration_advice = (
                f"{within_target_pct:.0f}% <= {target_h:g}h target (P90 {p90_h:.1f}h); "
                f"consider loosening via --set-slo {roller_name}={suggested_target:g}:{max(breach_h, round(suggested_target * 2)):g}."
            )
        elif within_target_pct >= 95.0 and p90_h < target_h * 0.65:
            tight_t = max(4.0, round((p90_h * 1.25) * 2.0) / 2.0)
            calibration_advice = (
                f"High compliance ({within_target_pct:.0f}% <= {target_h:g}h, P90 {p90_h:.1f}h); "
                f"can tighten via --set-slo {roller_name}={tight_t:g}:{round(tight_t * 2):g}."
            )
        else:
            calibration_advice = (
                f"Well-calibrated ({within_target_pct:.0f}% <= {target_h:g}h target, "
                f"{within_breach_pct:.0f}% <= {breach_h:g}h breach; P50 {p50_h:.1f}h, P90 {p90_h:.1f}h)."
            )

    stats = {
        "sample_rolls": sample_rolls,
        "p50_hours": p50_h,
        "p90_hours": p90_h,
        "max_hours": max_h,
        "within_target_pct": within_target_pct,
        "within_breach_pct": within_breach_pct,
        "first_try_pct": first_try_pct,
        "avg_attempts": avg_attempts,
        "sample_builds": sample_builds,
        "build_success_pct": build_success_pct,
        "calibration_advice": calibration_advice,
    }
    r_hist["stats"] = stats

    # Evaluate live SLO health state
    pending_count = int(p_info.get("pending_count", 0))
    oldest_ep = p_info.get("oldest_pending_epoch")
    lag_hours = (
        round(max(0.0, (now_epoch - float(oldest_ep)) / 3600.0), 1)
        if (pending_count > 0 and oldest_ep)
        else 0.0
    )

    consec_Build_failures = 0
    if pending_count > 0:
        for b in recent_builds:
            st = b.get("status", "")
            if st in ("STARTED", "SCHEDULED"):
                continue
            if st in ("FAILURE", "INFRA_FAILURE"):
                consec_Build_failures += 1
            elif st == "SUCCESS":
                break

    actionable_list = (
        (failure_classification or {}).get("actionable", [])
        if pending_count > 0
        else []
    )
    actionable_count = len(actionable_list)
    active_cq_failed = (
        int((pending_attempt_ui.get("cq") or {}).get("failed", 0))
        if (pending_count > 0 and pending_attempt_ui)
        else 0
    )

    if pending_count > 0 and (
        actionable_count > 0
        or lag_hours > breach_h
        or consec_Build_failures >= 3
    ):
        status = "breach"
        label = "SLO Breached"
        icon = "🔴"
        reasons = []
        if actionable_count > 0:
            reasons.append(f"{actionable_count} actionable failure(s)")
        if lag_hours > breach_h:
            reasons.append(f"lag ~{lag_hours:.1f}h > {breach_h:g}h breach")
        if consec_Build_failures >= 3:
            reasons.append(
                f"{consec_Build_failures} consecutive failed attempts"
            )
        reason = "; ".join(reasons)
        summary_text = f"Lag ~{lag_hours:.1f}h / {breach_h:g}h max ({reason})"
    elif pending_count > 0 and (
        lag_hours > target_h
        or consec_Build_failures == 2
        or active_cq_failed > 0
    ):
        status = "warning"
        label = "SLO At Risk"
        icon = "🟡"
        reasons = []
        if lag_hours > target_h:
            reasons.append(
                f"lag ~{lag_hours:.1f}h > {target_h:g}h target (breach >{breach_h:g}h)"
            )
        if consec_Build_failures == 2:
            reasons.append("2 consecutive failed attempts")
        if active_cq_failed > 0:
            reasons.append(f"{active_cq_failed} active CQ failure(s)")
        reason = "; ".join(reasons)
        summary_text = f"Lag ~{lag_hours:.1f}h / ≤{target_h:g}h SLO ({reason})"
    else:
        status = "healthy"
        label = "Within SLO"
        icon = "🟢"
        if pending_count == 0:
            reason = f"0 pending commits (target ≤{target_h:g}h)"
            summary_text = f"0h lag / ≤{target_h:g}h SLO"
        else:
            reason = (
                f"unrolled lag ~{lag_hours:.1f}h within ≤{target_h:g}h target"
            )
            summary_text = f"Lag ~{lag_hours:.1f}h / ≤{target_h:g}h SLO"

    return {
        "status": status,
        "label": label,
        "icon": icon,
        "target_hours": target_h,
        "breach_hours": breach_h,
        "target_compliance_pct": target_comp_pct,
        "lag_hours": lag_hours,
        "oldest_pending_epoch": oldest_ep if pending_count > 0 else None,
        "consecutive_failures": consec_Build_failures,
        "actionable_failures": actionable_count,
        "active_cq_failures": active_cq_failed,
        "reason": reason,
        "summary_text": summary_text,
        "stats": stats,
    }


def render_markdown_status_table(
    ui_rollers: list[dict],
    watched_cls: list[dict] | None = None,
    now_sec: float | None = None,
) -> str:
    """Render the concise 4-column Markdown status table with SLO indicators and approximate local + relative times."""
    now = now_sec if now_sec is not None else time.time()
    lines = [
        "| Roller / CL & SLO Health | Pinned → Head (Pending) & Last Landed | Pending Attempt | Last Failed Attempt |",
        "| :--- | :--- | :--- | :--- |",
    ]
    for r in ui_rollers:
        name = r["name"]
        slo = r.get("slo") or {}
        slo_icon = slo.get("icon", "🟢")
        slo_label = slo.get("label", "Within SLO")
        slo_summary = slo.get("summary_text", "")
        slo_stats = slo.get("stats") or {}
        stats_line = ""
        if slo_stats.get("sample_rolls"):
            stats_line = (
                f"<br>*(P50 {slo_stats['p50_hours']:.1f}h · "
                f"P90 {slo_stats['p90_hours']:.1f}h · "
                f"{slo_stats['within_target_pct']:.0f}% ≤{slo.get('target_hours', 12):g}h)*"
            )
        col1_roller = (
            f"{slo_icon} **`{name}`** -- **{slo_label}**<br>"
            f"{slo_summary}{stats_line}"
        )

        pinned = r["pinned_revision"]
        head = r["head_revision"]
        p_count = r["pending_count"]
        if p_count > 0:
            rev_str = f"`{pinned}` → `{head}` (**{p_count} pending**)"
            if slo.get("oldest_pending_epoch"):
                rev_str += f"<br>Oldest pending: {format_time_pair(slo['oldest_pending_epoch'], now)}"
        else:
            rev_str = f"`{pinned}` (**0 pending**, up to date)"

        landed = r.get("last_landed") or {}
        if landed.get("epoch"):
            t_str = format_time_pair(landed["epoch"], now)
            cl_part = (
                f" via [fxr/{landed['cl_num']}]({landed['cl_url']})"
                if landed.get("cl_url")
                else ""
            )
            sha_part = (
                f" (`{landed['commit']}`)" if landed.get("commit") else ""
            )
            rev_str += f"<br>Landed {t_str}{cl_part}{sha_part}"

        pa = r.get("pending_attempt")
        if pa:
            cl_md = (
                f"[fxr/{pa['cl_num']}/{pa.get('patchset') or '1'}]({pa['cl_url']})"
                if pa.get("cl_url")
                else "*creating CL*"
            )
            b_md = (
                f"[`{pa['build_id']}`]({pa['build_url']})"
                if pa.get("build_url")
                else "build"
            )
            t_md = (
                f" {format_time_pair(pa['start_epoch'], now)}"
                if pa.get("start_epoch")
                else ""
            )
            cq = pa.get("cq")
            cq_md = (
                f", `{cq['passed']}/{cq['total']}` passed, `{cq['failed']}` failed, `{cq['running']}` running"
                if cq and cq.get("total")
                else ""
            )
            col3 = f"{cl_md} ({b_md}): `{pa['status']}`{t_md}, {pa.get('attempt_kind', '')}{cq_md}"
        else:
            col3 = "--"

        lf = r.get("last_failed_attempt")
        if p_count > 0 and lf:
            cl_md = (
                f"[fxr/{lf['cl_num']}/{lf.get('patchset') or '1'}]({lf['cl_url']})"
                if lf.get("cl_url") and lf.get("cl_num")
                else "no CL"
            )
            b_md = (
                f"[`{lf['build_id']}`]({lf['build_url']})"
                if lf.get("build_url")
                else "build"
            )
            t_md = (
                f" {format_time_pair(lf['end_epoch'], now)}"
                if lf.get("end_epoch")
                else ""
            )
            summary_str = (lf.get("failure_summary") or "").replace("|", "\\|")
            col4 = f"{cl_md} ({b_md}): ended{t_md}, {lf.get('attempt_kind', '')} -- {summary_str}"
        else:
            col4 = "-- *(up to date)*"

        lines.append(f"| {col1_roller} | {rev_str} | {col3} | {col4} |")

    for sc in watched_cls or []:
        if not sc:
            continue
        ps_suffix = f"/{sc['patchset']}" if sc.get("patchset") else ""
        col1 = f"**[fxr/{sc['cl_num']}{ps_suffix}]({sc['cl_url']})**<br>({sc.get('subject', '')})"
        up_t = (
            f" (PS{sc['patchset']} uploaded {format_time_pair(sc['ps_created_epoch'], now)})"
            if sc.get("ps_created_epoch")
            else ""
        )
        col2 = f"`{sc.get('status', 'UNKNOWN')}`{up_t}"
        cq = sc.get("cq")
        cq_md = (
            f"`{cq['passed']}/{cq['total']}` CQ tryjobs passed (`{cq['failed']}` failed, `{cq['running']}` running)"
            if cq and cq.get("total")
            else "No active CQ run"
        )
        col3 = cq_md
        col4 = f"`{sc.get('unresolved_comment_count', 0)}` unresolved comments"
        lines.append(f"| {col1} | {col2} | {col3} | {col4} |")

    return "\n".join(lines)


def collect_rollers_snapshot(
    fuchsia_root: Path,
    worktree_dir: Path,
    state_path: Path,
    watched_cl_nums: list[str] | None = None,
    landed_cache: dict[tuple[str, str], dict] | None = None,
    run_monitor: bool = False,
    update_skill: bool = False,
) -> tuple[dict, dict, list[str]]:
    """Collect full roller report, UI snapshot dict, and skill sync messages."""
    rollers, pending = discover_rollers_and_pending(fuchsia_root)
    monitor_state = load_monitor_state(state_path)
    history_path = state_path.parent / "roller_health_history.json"
    health_history = load_health_history(history_path)
    tree_info = fetch_tree_status(fuchsia_root)
    now_epoch = time.time()

    report: dict[str, dict] = {}
    ui_rollers: list[dict] = []

    for name, info in rollers.items():
        builds = fetch_recent_builds(fuchsia_root, info["builder"], count=3)
        update_scheduler_job_from_builds(info, builds)

        step_cache: dict[str, dict] = {}
        cl_builds_cache: dict[tuple[str, str | None], list[dict]] = {}
        for b in builds:
            bid = b.get("id")
            if bid and b.get("status") in (
                "STARTED",
                "FAILURE",
                "INFRA_FAILURE",
            ):
                step_cache[bid] = inspect_roll_build_steps(fuchsia_root, bid)

        for idx, b in enumerate(builds):
            bid = b.get("id")
            if bid in step_cache:
                prev_info = None
                for older in builds[idx + 1 :]:
                    older_id = older.get("id")
                    if older_id in step_cache:
                        prev_info = step_cache[older_id]
                        break
                b["attempt_kind"] = describe_roll_attempt(
                    step_cache[bid], prev_info
                )
                b_cl = step_cache[bid].get("cl_num")
                b_ps = step_cache[bid].get("patchset")
                if b_cl:
                    cache_key = (b_cl, b_ps)
                    if cache_key not in cl_builds_cache:
                        _, fetched_builds, cl_err = fetch_cl_builds(
                            fuchsia_root, b_cl, b_ps
                        )
                        cl_builds_cache[cache_key] = (
                            [] if cl_err is not None else fetched_builds
                        )
                    b_cls = classify_roll_failures(
                        name,
                        b,
                        step_cache[bid],
                        cl_builds_cache[cache_key],
                    )
                    b["flake_signatures"] = b_cls.get("flake_signatures", [])

        latest = builds[0] if builds else {}
        latest_id = latest.get("id")
        latest_status = latest.get("status", "UNKNOWN")
        latest_steps = step_cache.get(latest_id, {})
        cl_num = latest_steps.get("cl_num")
        patchset = latest_steps.get("patchset")
        triage_md = None
        cl_builds: list[dict] = []
        if cl_num and latest_status in (
            "STARTED",
            "FAILURE",
            "INFRA_FAILURE",
        ):
            cache_key = (cl_num, patchset)
            if cache_key in cl_builds_cache:
                cl_builds = cl_builds_cache[cache_key]
                triage_md, _ = run_triage_cl(
                    fuchsia_root, cl_num, patchset, builds=cl_builds
                )
            else:
                _, cl_builds, cl_err = fetch_cl_builds(
                    fuchsia_root, cl_num, patchset
                )
                if cl_err is not None:
                    triage_md = cl_err
                else:
                    triage_md, _ = run_triage_cl(
                        fuchsia_root, cl_num, patchset, builds=cl_builds
                    )

        failure_classification = None
        if latest_status in ("STARTED", "FAILURE", "INFRA_FAILURE"):
            failure_classification = classify_roll_failures(
                name, latest, latest_steps, cl_builds
            )

        last_failed = fetch_last_failed_build(
            fuchsia_root, info["builder"], builds
        )
        last_failed_info = None
        last_failed_ui = None
        if last_failed:
            f_id = last_failed.get("id")
            f_steps = step_cache.get(f_id) or inspect_roll_build_steps(
                fuchsia_root, f_id
            )
            f_cl = f_steps.get("cl_num")
            f_ps = f_steps.get("patchset")
            f_prev_steps = None
            for idx, b in enumerate(builds):
                if b.get("id") == f_id:
                    for older in builds[idx + 1 :]:
                        if older.get("id") in step_cache:
                            f_prev_steps = step_cache[older["id"]]
                            break
                    break
            f_attempt_kind = describe_roll_attempt(f_steps, f_prev_steps)
            f_gerrit_url = (
                f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{f_cl}"
                + (f"/{f_ps}" if f_ps else "")
                if f_cl
                else extract_gerrit_url_from_summary(
                    last_failed.get("summaryMarkdown", "")
                )
            )
            f_triage_md = None
            f_brief = _first_meaningful_summary_line(
                last_failed.get("summaryMarkdown", "")
            )
            f_flake_sigs: list[str] = []
            if f_cl:
                cache_key = (f_cl, f_ps)
                f_cl_builds = cl_builds_cache.get(cache_key)
                if f_cl_builds is None:
                    _, f_cl_builds, f_err = fetch_cl_builds(
                        fuchsia_root, f_cl, f_ps
                    )
                    if f_err is not None:
                        f_cl_builds = []
                f_triage_md, f_brief = run_triage_cl(
                    fuchsia_root, f_cl, f_ps, builds=f_cl_builds
                )
                f_cls = classify_roll_failures(
                    name, last_failed, f_steps, f_cl_builds
                )
                f_flake_sigs = f_cls.get("flake_signatures", [])
            elif f_gerrit_url:
                f_brief = f"{f_gerrit_url} ({f_brief})"
            last_failed_info = {
                "build_id": f_id,
                "build_url": f"https://cr-buildbucket.appspot.com/build/{f_id}",
                "status": last_failed.get("status"),
                "create_time": last_failed.get("createTime", ""),
                "end_time": last_failed.get("endTime", ""),
                "gerrit_url": f_gerrit_url,
                "attempt_kind": f_attempt_kind,
                "brief_summary": f_brief,
                "triage_report": f_triage_md,
                "flake_signatures": f_flake_sigs,
            }
            last_failed_ui = {
                "build_id": f_id,
                "build_url": f"https://cr-buildbucket.appspot.com/build/{f_id}",
                "start_epoch": _iso_to_epoch(last_failed.get("createTime")),
                "end_epoch": _iso_to_epoch(
                    last_failed.get("endTime") or last_failed.get("createTime")
                ),
                "cl_num": f_cl,
                "patchset": f_ps,
                "cl_url": f_gerrit_url,
                "attempt_kind": f_attempt_kind,
                "failure_summary": f_brief,
            }

        pending_attempt_ui = None
        if latest_status == "STARTED":
            cl_url = (
                f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{cl_num}/{patchset}"
                if (cl_num and patchset)
                else (
                    f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{cl_num}"
                    if cl_num
                    else None
                )
            )
            cq_summary = (
                _summarize_cq_builds(fuchsia_root, cl_num, patchset, cl_builds)
                if (cl_num and cl_builds)
                else None
            )
            pending_attempt_ui = {
                "build_id": latest_id,
                "build_url": (
                    f"https://cr-buildbucket.appspot.com/build/{latest_id}"
                    if latest_id
                    else None
                ),
                "status": "STARTED",
                "start_epoch": _iso_to_epoch(latest.get("createTime")),
                "cl_num": cl_num,
                "patchset": patchset,
                "cl_url": cl_url,
                "attempt_kind": latest.get("attempt_kind") or "in progress",
                "cq": cq_summary,
            }

        p_info = pending.get(name, {})
        pinned_rev = p_info.get("pinned_revision", "")
        head_rev = p_info.get("upstream_head", "")
        pending_count = p_info.get("pending_count", 0)
        pending_commits = [
            {
                "sha": c["short_sha"],
                "subject": c["subject"],
                "epoch": c.get("epoch"),
            }
            for c in p_info.get("pending_commits", [])
        ]

        last_landed = fetch_last_landed_roll(
            fuchsia_root,
            name,
            info["manifest_project"],
            pinned_rev,
            cache=landed_cache,
        )

        r_state = monitor_state.get("flake_streaks", {}).get(name, {})
        backoff_ui = None
        if r_state.get("streak_count", 0) >= 2:
            backoff_ui = {
                "streak": r_state.get("streak_count", 0),
                "signatures": r_state.get("signatures", []),
            }

        slo_eval = update_and_evaluate_roller_slo(
            history=health_history,
            roller_name=name,
            manifest_project=info["manifest_project"],
            schedule=info["schedule"],
            p_info=p_info,
            last_landed=last_landed,
            recent_builds=builds,
            failure_classification=failure_classification,
            pending_attempt_ui=pending_attempt_ui,
            backoff_ui=backoff_ui,
            now_epoch=now_epoch,
        )

        proj = info.get("luci_project", "fuchsia")
        bucket = info.get("bucket", "global.roller")
        job_id = info.get("scheduler_job", "")

        ui_rollers.append(
            {
                "name": name,
                "manifest_project": info["manifest_project"],
                "path": info["path"],
                "schedule": info["schedule"],
                "builder_url": f"https://ci.chromium.org/p/{proj}/builders/{bucket}/{name}",
                "scheduler_url": f"https://luci-scheduler.appspot.com/jobs/{proj}/{job_id}",
                "pinned_revision": pinned_rev[:12],
                "head_revision": head_rev[:12],
                "pending_count": pending_count,
                "pending_commits": pending_commits,
                "last_landed": last_landed,
                "pending_attempt": pending_attempt_ui,
                "last_failed_attempt": last_failed_ui,
                "backoff": backoff_ui,
                "slo": slo_eval,
            }
        )

        report[name] = {
            **info,
            "pending": p_info,
            "last_landed": last_landed,
            "slo": slo_eval,
            "recent_builds": builds,
            "active_or_failed_cl": (
                f"https://fuchsia-review.googlesource.com/c/fuchsia/+/{cl_num}"
                + (f"/{patchset}" if patchset else "")
                if cl_num
                else None
            ),
            "active_attempt_kind": latest.get("attempt_kind"),
            "triage_report": triage_md,
            "failure_classification": failure_classification,
            "last_failed_run": last_failed_info,
        }

    try:
        save_health_history(history_path, health_history)
    except Exception:
        pass

    for name, data in report.items():
        cls = data.get("failure_classification") or {}
        if not cls.get("actionable") and cls.get("flakes"):
            data["retrigger_policy"] = evaluate_flake_retrigger_policy(
                fuchsia_root,
                name,
                data,
                report,
                tree_info,
                monitor_state,
            )
        else:
            data["retrigger_policy"] = None

    skill_updates = sync_skill_files(
        rollers, force_write=(update_skill or run_monitor)
    )

    if run_monitor:
        run_monitor_actions(
            fuchsia_root,
            worktree_dir,
            rollers,
            report,
            monitor_state,
            state_path,
        )

    watched_cls = []
    for cl_id in watched_cl_nums or []:
        cl_status = fetch_watched_cl_status(fuchsia_root, cl_id)
        if cl_status:
            watched_cls.append(cl_status)

    markdown_table = render_markdown_status_table(
        ui_rollers, watched_cls=watched_cls, now_sec=now_epoch
    )
    ui_snapshot = {
        "updated_epoch": now_epoch,
        "tree_status": {
            "tree": tree_info.get("tree", "fuchsia-stem"),
            "state": tree_info.get("general_state", "UNKNOWN"),
            "message": tree_info.get("message", ""),
            "epoch": _iso_to_epoch(tree_info.get("create_time")),
            "last_reopened_epoch": _iso_to_epoch(
                tree_info.get("last_reopened_at")
            ),
        },
        "rollers": ui_rollers,
        "watched_cls": watched_cls,
        "skill_cl": watched_cls[0] if watched_cls else None,
        "markdown_table": markdown_table,
    }
    return report, ui_snapshot, skill_updates


class RollersSidecarState:
    """Thread-safe state container and background monitor loop for --ui-server."""

    def __init__(
        self,
        fuchsia_dir: Path,
        worktree_dir: Path,
        state_file: Path,
        ui_cache_file: Path,
        html_path: Path,
        watched_cl_nums: list[str] | None = None,
    ):
        self.fuchsia_dir = fuchsia_dir
        self.worktree_dir = worktree_dir
        self.state_file = state_file
        self.ui_cache_file = ui_cache_file
        self.html_path = html_path
        self.watched_cl_nums = watched_cl_nums or []
        self._lock = threading.Lock()
        self._refresh_event = threading.Event()
        self._stop_event = threading.Event()
        self._is_refreshing = False
        self._last_monitor_action_epoch = 0.0
        self._landed_cache: dict[tuple[str, str], dict] = {}
        self._snapshot: dict = self._load_initial_cache()

    def _load_initial_cache(self) -> dict:
        if self.ui_cache_file.is_file():
            try:
                return json.loads(
                    self.ui_cache_file.read_text(encoding="utf-8")
                )
            except Exception:
                pass
        return {
            "updated_epoch": None,
            "tree_status": {
                "tree": "fuchsia-stem",
                "state": "UNKNOWN",
                "message": "Initializing...",
            },
            "rollers": [],
            "watched_cls": [],
            "skill_cl": None,
        }

    def get_snapshot(self) -> dict:
        with self._lock:
            data = dict(self._snapshot)
            data["is_refreshing"] = self._is_refreshing
            data["server_time_epoch"] = time.time()
            try:
                data["ui_mtime"] = self.html_path.stat().st_mtime
            except OSError:
                data["ui_mtime"] = 0
            return data

    def trigger_refresh(self) -> None:
        self._refresh_event.set()

    def stop(self) -> None:
        self._stop_event.set()
        self._refresh_event.set()

    def retrigger_roller_from_ui(self, roller_name: str) -> dict:
        rollers, _ = discover_rollers_and_pending(self.fuchsia_dir)
        if roller_name not in rollers:
            raise ValueError(f"Unknown roller: {roller_name}")
        info = rollers[roller_name]
        builds = fetch_recent_builds(self.fuchsia_dir, info["builder"], count=1)
        update_scheduler_job_from_builds(info, builds)
        resp = retrigger_roller(
            self.fuchsia_dir,
            rollers,
            roller_name,
            "Manual retrigger from Libc Rollers UI panel",
        )
        self.trigger_refresh()
        return {
            "ok": True,
            "output": json.dumps(resp),
            "job_id": f"{info['luci_project']}/{info['scheduler_job']}",
        }

    def poll_once(self, force_monitor: bool = False) -> None:
        with self._lock:
            if self._is_refreshing:
                return
            self._is_refreshing = True

        try:
            now_epoch = time.time()
            should_run_monitor = force_monitor or (
                now_epoch - self._last_monitor_action_epoch >= 900
            )
            _, new_snapshot, _ = collect_rollers_snapshot(
                self.fuchsia_dir,
                self.worktree_dir,
                self.state_file,
                watched_cl_nums=self.watched_cl_nums,
                landed_cache=self._landed_cache,
                run_monitor=should_run_monitor,
            )
            if should_run_monitor:
                self._last_monitor_action_epoch = now_epoch

            with self._lock:
                self._snapshot = new_snapshot
            try:
                self.ui_cache_file.parent.mkdir(parents=True, exist_ok=True)
                self.ui_cache_file.write_text(
                    json.dumps(new_snapshot, indent=2) + "\n", encoding="utf-8"
                )
            except Exception:
                pass
        except Exception:
            traceback.print_exc(file=sys.stderr)
        finally:
            with self._lock:
                self._is_refreshing = False

    def run_background_loop(self) -> None:
        first_run = True
        while not self._stop_event.is_set():
            self.poll_once(force_monitor=first_run)
            first_run = False

            snap = self.get_snapshot()
            has_active = any(
                r.get("pending_attempt") is not None
                for r in snap.get("rollers", [])
            ) or any(
                c.get("status") == "NEW" for c in snap.get("watched_cls", [])
            )
            wait_sec = 180.0 if has_active else 900.0
            self._refresh_event.wait(timeout=wait_sec)
            self._refresh_event.clear()


def _make_ui_http_handler(
    state: RollersSidecarState, html_path: Path, token: str | None
):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, fmt, *args):
            pass

        def _validate_token(self) -> bool:
            if not token:
                return True
            provided = self.headers.get("X-Sidecar-Token")
            if not provided:
                parsed = urlparse(self.path)
                provided = parse_qs(parsed.query).get("token", [None])[0]
            if not provided or not isinstance(provided, str):
                return False
            try:
                return hmac.compare_digest(provided, token)
            except TypeError:
                return False

        def _send_json(self, payload: dict, status: int = 200) -> None:
            body = json.dumps(payload).encode("utf-8")
            try:
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                self.close_connection = True

        def _send_text(
            self, text: str, content_type: str, status: int = 200
        ) -> None:
            body = text.encode("utf-8")
            try:
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                self.close_connection = True

        def do_GET(self):
            parsed = urlparse(self.path)
            path = parsed.path
            if path == "/preload.js":
                self._send_text(
                    PRELOAD_JS_TEXT, "application/javascript; charset=utf-8"
                )
                return

            if path in ("/", "/index.html"):
                html = html_path.read_text(encoding="utf-8")
                self._send_text(html, "text/html; charset=utf-8")
                return

            if not self._validate_token():
                self._send_json({"error": "unauthorized"}, status=401)
                return

            if path == "/api/status":
                self._send_json(state.get_snapshot())
                return

            self._send_json({"error": "not found"}, status=404)

        def do_POST(self):
            if not self._validate_token():
                self._send_json({"error": "unauthorized"}, status=401)
                return

            parsed = urlparse(self.path)
            path = parsed.path
            length = int(self.headers.get("Content-Length", "0"))
            raw_body = self.rfile.read(length) if length > 0 else b""
            req_data = json.loads(raw_body.decode("utf-8")) if raw_body else {}

            if path == "/api/refresh":
                state.trigger_refresh()
                self._send_json({"ok": True, "is_refreshing": True})
                return

            if path == "/api/retrigger":
                roller_name = req_data.get("roller", "")
                try:
                    res = state.retrigger_roller_from_ui(roller_name)
                    self._send_json(res)
                except Exception as exc:
                    self._send_json(
                        {"ok": False, "error": str(exc)}, status=400
                    )
                return

            self._send_json({"error": "not found"}, status=404)

    return Handler


def run_ui_server(
    fuchsia_root: Path,
    worktree_dir: Path,
    state_path: Path,
    watched_cl_nums: list[str] | None = None,
) -> None:
    """Start the AuxPane Web UI & background monitor HTTP server."""
    port_str = os.environ.get("ANTIGRAVITY_SIDECAR_WEB_PORT", "38987")
    port = int(port_str)
    token = os.environ.get("ANTIGRAVITY_SIDECAR_UI_TOKEN")

    data_dir = state_path.parent
    data_dir.mkdir(parents=True, exist_ok=True)
    ui_cache_file = data_dir / "ui_status.json"

    skill_html = (
        Path(__file__).resolve().parent.parent / "resources" / "index.html"
    )
    fallback_html = (
        Path.home()
        / ".gemini"
        / "config"
        / "sidecars"
        / "libc-rollers-monitor"
        / "index.html"
    )
    html_path = skill_html if skill_html.is_file() else fallback_html

    state = RollersSidecarState(
        fuchsia_dir=fuchsia_root,
        worktree_dir=worktree_dir,
        state_file=state_path,
        ui_cache_file=ui_cache_file,
        html_path=html_path,
        watched_cl_nums=watched_cl_nums,
    )

    bg_thread = threading.Thread(target=state.run_background_loop, daemon=True)
    bg_thread.start()

    handler_cls = _make_ui_http_handler(state, html_path, token)
    server = ThreadingHTTPServer((UI_BIND_HOST, port), handler_cls)
    print(
        f"[libc-rollers-monitor] UI & monitor server listening on http://{UI_BIND_HOST}:{port}",
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        state.stop()
        server.server_close()


def install_sidecar(
    fuchsia_root: Path,
    worktree_dir: Path,
    watched_cl_nums: list[str] | None = None,
    sidecar_id: str = "libc-rollers-monitor",
) -> Path:
    """Install the live AuxPane Web UI & background monitor sidecar for the current user."""
    script_rel = Path(
        "sdk/lib/c/skills/libc-rollers/scripts/check_libc_rollers.py"
    )
    script_path = worktree_dir / script_rel
    if not script_path.exists():
        script_path = fuchsia_root / script_rel

    bb_bin = fuchsia_root / "prebuilt" / "tools" / "buildbucket" / "bb"
    rdb_bin = fuchsia_root / "prebuilt" / "tools" / "resultdb" / "rdb"
    luci_auth_bin = (
        fuchsia_root / "prebuilt" / "tools" / "luci-auth" / "luci-auth"
    )

    args_list = [
        str(script_path),
        "--ui-server",
        "--fuchsia-dir",
        str(fuchsia_root),
        "--worktree-dir",
        str(worktree_dir),
    ]
    for cl_num in watched_cl_nums or []:
        if re.fullmatch(r"[0-9]+", str(cl_num)):
            args_list.extend(["--watch-cl", str(cl_num)])

    sidecar_config = {
        "command": "/usr/bin/python3",
        "args": args_list,
        "restart_policy": "always",
        "has_web_ui": True,
        "display_name": "Libc Rollers Monitor",
        "description": (
            "Live AuxPane status table and background monitor for llvm-libc-roller, "
            "scudo-roller, and gwp_asan-roller. Silently auto-retriggers unrelated "
            f"Fuchsia flakes and only spawns a new conversation in {worktree_dir} "
            "when a non-flake failure needs attention."
        ),
        "ui_config": {
            "views": [
                {
                    "title": "Libc Rollers",
                    "path": "/",
                    "entrypoint": "SIDECAR_UI_ENTRYPOINT_AUX_PANE",
                    "icon": (
                        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" '
                        'fill="none" stroke="currentColor" stroke-width="2" '
                        'stroke-linecap="round" stroke-linejoin="round">'
                        '<path d="M21 12a9 9 0 0 0-9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/>'
                        '<path d="M3 3v5h5"/>'
                        '<path d="M3 12a9 9 0 0 0 9 9 9.75 9.75 0 0 0 6.74-2.74L21 16"/>'
                        '<path d="M16 16h5v5"/></svg>'
                    ),
                }
            ]
        },
        "agent_permissions": {
            "workspace_uris": [],
            "access_grants": [
                f"command({bb_bin} ls)",
                f"command({bb_bin} get)",
                f"command({bb_bin} log)",
                f"command({rdb_bin} query)",
                f"command({luci_auth_bin} token)",
                f"read_file({fuchsia_root})",
                f"read_file({worktree_dir})",
                "read_url(fuchsia.googlesource.com)",
                "read_url(fuchsia-review.googlesource.com)",
                "read_url(llvm.googlesource.com)",
            ],
        },
    }

    sidecar_dir = Path.home() / ".gemini" / "config" / "sidecars" / sidecar_id
    sidecar_dir.mkdir(parents=True, exist_ok=True)
    sidecar_path = sidecar_dir / "sidecar.json"
    sidecar_path.write_text(json.dumps(sidecar_config, indent=2) + "\n")

    skill_html = (
        Path(__file__).resolve().parent.parent / "resources" / "index.html"
    )
    if skill_html.is_file():
        shutil.copyfile(skill_html, sidecar_dir / "index.html")

    ls_addr = os.environ.get("ANTIGRAVITY_LS_ADDRESS")
    csrf = os.environ.get("ANTIGRAVITY_CSRF_TOKEN")
    if ls_addr and csrf:
        for action_payload in (
            {
                "sidecarId": sidecar_id,
                "action": "SIDECAR_ACTION_UPDATE",
                "config": sidecar_config,
            },
            {
                "sidecarId": sidecar_id,
                "action": "SIDECAR_ACTION_RESTART",
            },
        ):
            try:
                req = urllib.request.Request(
                    f"http://{ls_addr}/exa.language_server_pb.LanguageServerService/ManageSidecar",
                    data=json.dumps(action_payload).encode("utf-8"),
                    headers={
                        "Content-Type": "application/json",
                        "x-codeium-csrf-token": csrf,
                    },
                    method="POST",
                )
                with urllib.request.urlopen(req, timeout=5):
                    pass
            except Exception:
                pass

    return sidecar_path


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Monitor, triage, retrigger, and self-update Fuchsia libc rollers."
    )
    parser.add_argument(
        "--fuchsia-dir",
        help="Path to the reference Fuchsia checkout (auto-detected if omitted).",
    )
    parser.add_argument(
        "--worktree-dir",
        help="Dedicated Jiri worktree path (auto-detected or <fuchsia-dir>-libc-rollers if omitted).",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Output status report as JSON.",
    )
    parser.add_argument(
        "--table-only",
        action="store_true",
        help="Output only the concise 4-column Markdown status table.",
    )
    parser.add_argument(
        "--retrigger",
        action="append",
        help="Retrigger the specified roller (e.g., llvm-libc-roller, scudo-roller, gwp_asan-roller).",
    )
    parser.add_argument(
        "--reason",
        default="Retrigger after unrelated Fuchsia flake",
        help="Reason string recorded when retriggering.",
    )
    parser.add_argument(
        "--update-skill",
        action="store_true",
        help="Self-update SKILL.md and DEFAULT_ROLLERS if any LUCI builder, bucket, URL, or schedule details changed.",
    )
    parser.add_argument(
        "--monitor",
        action="store_true",
        help=(
            "Run scheduled monitoring: log status, silently auto-retrigger unrelated flakes, "
            "and only spawn a new conversation (titled with the specific issue) when an "
            "actionable failure needs attention."
        ),
    )
    parser.add_argument(
        "--ui-server",
        action="store_true",
        help="Run the live AuxPane Web UI & background monitor server for the sidecar panel.",
    )
    parser.add_argument(
        "--watch-cl",
        action="append",
        default=[],
        help="Optional Gerrit CL number(s) to include in the status table and UI panel.",
    )
    parser.add_argument(
        "--state-file",
        help="Path to persistent monitor state JSON file for --monitor deduplication.",
    )
    parser.add_argument(
        "--set-slo",
        action="append",
        default=[],
        metavar="ROLLER=TARGET_H:BREACH_H",
        help=(
            "Update per-roller SLO thresholds in roller_health_history.json "
            "(e.g., --set-slo llvm-libc-roller=12:24 --set-slo scudo-roller=18:36)."
        ),
    )
    parser.add_argument(
        "--install-sidecar",
        action="store_true",
        help="Install and start the live Libc Rollers AuxPane UI & monitor sidecar for this user.",
    )
    parser.add_argument(
        "--schedule",
        default="CRON_TZ=America/Los_Angeles 0 * * * *",
        help="Deprecated (kept for backwards compatibility with --install-sidecar).",
    )
    args = parser.parse_args()

    fuchsia_root, wt_dir = resolve_checkout_and_worktree_dirs(
        args.fuchsia_dir, args.worktree_dir
    )

    default_data_dir = Path(
        os.environ.get(
            "ANTIGRAVITY_EXECUTABLE_DATA_DIR",
            str(
                Path.home()
                / ".gemini"
                / "jetski"
                / "sidecar_data"
                / "libc-rollers-monitor"
                / "data"
            ),
        )
    )
    state_path = (
        Path(args.state_file).resolve()
        if args.state_file
        else default_data_dir / "monitor_state.json"
    )
    history_path = state_path.parent / "roller_health_history.json"

    if args.set_slo:
        updated_cfg = apply_slo_overrides(history_path, args.set_slo)
        print(f"Updated SLO configuration in {history_path}:")
        for r_name, cfg in updated_cfg.items():
            print(
                f"  - {r_name}: target <= {cfg['target_hours']:g}h, "
                f"breach > {cfg['breach_hours']:g}h"
            )
        if (
            not args.table_only
            and not args.json
            and not args.monitor
            and not args.install_sidecar
            and not args.ui_server
        ):
            return 0

    if args.install_sidecar:
        out_path = install_sidecar(
            fuchsia_root, wt_dir, watched_cl_nums=args.watch_cl
        )
        print(f"Installed live Libc Rollers sidecar to {out_path}")
        print(f"  Reference checkout (read-only): {fuchsia_root}")
        print(f"  Dedicated worktree (read/write): {wt_dir}")
        print(
            "  Open in chat or AuxPane: [Libc Rollers](sidecar://libc-rollers-monitor/)"
        )
        return 0

    if args.ui_server:
        run_ui_server(
            fuchsia_root,
            wt_dir,
            state_path,
            watched_cl_nums=args.watch_cl,
        )
        return 0

    if args.retrigger:
        rollers, _ = discover_rollers_and_pending(fuchsia_root)
        for r in args.retrigger:
            resp = retrigger_roller(fuchsia_root, rollers, r, args.reason)
            print(
                f"Triggered {r} ({rollers[r]['luci_project']}/{rollers[r]['scheduler_job']}): "
                f"{json.dumps(resp)}"
            )
        return 0

    report, ui_snapshot, skill_updates = collect_rollers_snapshot(
        fuchsia_root,
        wt_dir,
        state_path,
        watched_cl_nums=args.watch_cl,
        run_monitor=args.monitor,
        update_skill=args.update_skill,
    )

    if args.json and not args.monitor:
        print(
            json.dumps(
                {
                    "tree_status": ui_snapshot["tree_status"],
                    "rollers": report,
                    "ui_snapshot": ui_snapshot,
                    "markdown_table": ui_snapshot["markdown_table"],
                    "skill_updates": skill_updates,
                },
                indent=2,
            )
        )
        return 0

    if args.table_only:
        print(ui_snapshot["markdown_table"])
        return 0

    for msg in skill_updates:
        print(f"[SKILL SYNC] {msg}")

    t_info = ui_snapshot["tree_status"]
    print(
        f"=== Tree Status ({t_info['tree']}): {t_info['state']} "
        f"({t_info['message']}) ===\n"
    )
    print(ui_snapshot["markdown_table"])
    print()

    for name, data in report.items():
        p = data["pending"]
        pinned_short = p.get("pinned_revision", "unknown")[:12]
        head_short = p.get("upstream_head", "unknown")[:12]
        p_count = p.get("pending_count", 0)
        print(
            f"=== {name} ({data['manifest_project']}) | "
            f"job: {data['luci_project']}/{data['scheduler_job']} | "
            f"pinned: {pinned_short} | head: {head_short} | "
            f"pending: {p_count} commit(s) ==="
        )
        for c in p.get("pending_commits", []):
            print(f"  {c['short_sha']} {c['subject']}")
        if data.get("slo"):
            slo = data["slo"]
            st = slo.get("stats") or {}
            print(
                f"SLO Health: {slo['icon']} {slo['label']} -- {slo['summary_text']} "
                f"(target <= {slo['target_hours']:g}h, breach > {slo['breach_hours']:g}h)"
            )
            if st.get("sample_rolls"):
                print(
                    f"  Historical Convergence ({st['sample_rolls']} rolls): "
                    f"P50={st['p50_hours']:.1f}h, P90={st['p90_hours']:.1f}h, "
                    f"max={st['max_hours']:.1f}h, <=target={st['within_target_pct']:.0f}%, "
                    f"<=breach={st['within_breach_pct']:.0f}%, "
                    f"1st-try={st['first_try_pct']:.0f}% (avg {st['avg_attempts']:.1f} attempts)"
                )
                print(f"  SLO Calibration: {st.get('calibration_advice', '')}")
        print("Recent builds:")
        for b in data["recent_builds"]:
            bid = b.get("id")
            st = b.get("status")
            ct = b.get("createTime", "")
            et = b.get("endTime", "")
            ak = f" -- {b['attempt_kind']}" if b.get("attempt_kind") else ""
            print(
                f"  - https://cr-buildbucket.appspot.com/build/{bid} [{st}] ({ct} -> {et}){ak}"
            )
        if data.get("last_failed_run"):
            lf = data["last_failed_run"]
            print(
                f"Last failed run: {lf['build_url']} ({lf['end_time']}) "
                f"[{lf['attempt_kind']}] -> {lf['brief_summary']}"
            )
        if data["triage_report"]:
            ak = (
                f" ({data['active_attempt_kind']})"
                if data.get("active_attempt_kind")
                else ""
            )
            print(f"\nActive / Failed CL Triage{ak}:")
            print(data["triage_report"])
        if data.get("retrigger_policy"):
            pol = data["retrigger_policy"]
            persist_label = (
                f"persistent (streak={pol['streak_count']}, "
                f"cooldown={pol['cooldown_seconds'] // 60}m, "
                f"elapsed={pol['elapsed_seconds'] // 60}m)"
                if pol.get("is_persistent")
                else "transient (streak=1)"
            )
            action_label = (
                "RETRIGGER" if pol.get("should_retrigger") else "BACKOFF / HOLD"
            )
            print(
                f"\nRetrigger Policy: {action_label} [{persist_label}] "
                f"signatures={pol.get('signatures', [])}"
            )
            if pol.get("cross_roller_matches"):
                print(
                    f"  Cross-roller matches: {', '.join(pol['cross_roller_matches'])}"
                )
            for stream_name, s_info in (
                pol.get("ci_health", {}).get("streams", {}).items()
            ):
                print(
                    f"  CI {stream_name}: latest completed={s_info['latest_completed_status']} "
                    f"({s_info['latest_completed_id']} at {s_info['latest_completed_create_time']}) "
                    f"same_sig_failed={s_info['failed_with_same_signature']} "
                    f"green_since_roll={s_info['green_since_roll']}"
                )
            if pol.get("fix_signals"):
                print(f"  Fix signals: {'; '.join(pol['fix_signals'])}")
            if pol.get("backoff_reasons"):
                print(
                    f"  Hold/backoff reasons: {'; '.join(pol['backoff_reasons'])}"
                )
        print()

    return 0


if __name__ == "__main__":
    sys.exit(main())
