# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Checkout discovery, LUCI Scheduler config parsing, and upstream pending commits."""

import base64
import datetime
import json
import os
import pathlib
import re
import subprocess
import urllib.error
import xml.etree.ElementTree as ET
from collections.abc import Mapping, MutableMapping
from typing import Any

from libc_rollers import constants, luci, utils


def parse_default_luci_project(fuchsia_root: pathlib.Path) -> str:
    """Extract the roller LUCI project from override or //sdk/lib/c/README.md."""
    explicit_proj = utils.get_luci_project_override() or os.environ.get(
        "LUCI_PROJECT", ""
    )
    if explicit_proj and re.fullmatch(r"[a-z0-9_-]+", explicit_proj.strip()):
        return explicit_proj.strip()

    readme = fuchsia_root / "sdk" / "lib" / "c" / "README.md"
    if readme.exists():
        readme_text: str | None = readme.read_text(encoding="utf-8")
    else:
        readme_text = utils.fetch_remote_gob_text(constants.README_URL)
    if readme_text and (
        match := re.search(
            r"\[LUCI\]:\s*https://[^\s]+/p/([^/\s]+)/",
            readme_text,
        )
    ):
        return match.group(1)
    return "fuchsia"


def find_fuchsia_root(
    explicit_dir: pathlib.Path | None = None,
) -> pathlib.Path:
    """Locate the Fuchsia source checkout root if present, or fall back to cwd."""
    if explicit_dir is not None:
        return explicit_dir.expanduser().resolve()
    env_dir = os.environ.get("FUCHSIA_DIR")
    if env_dir and (pathlib.Path(env_dir) / ".jiri_root").exists():
        return pathlib.Path(env_dir).resolve()
    curr = pathlib.Path(__file__).resolve().parent
    for parent in (curr, *curr.parents):
        if (parent / "prebuilt" / "tools" / "buildbucket" / "bb").exists():
            return parent
        if (parent / "sdk" / "lib" / "c" / "manual-roll.sh").exists():
            dot_git = parent / ".git"
            if dot_git.is_file():
                try:
                    git_text = dot_git.read_text(encoding="utf-8").strip()
                    if match := re.match(
                        r"^gitdir:\s*(.+)/\.git/worktrees/[^/]+$", git_text
                    ):
                        main_checkout = (parent / match.group(1)).resolve()
                        if (
                            main_checkout
                            / "prebuilt"
                            / "tools"
                            / "buildbucket"
                            / "bb"
                        ).exists():
                            return main_checkout
                except OSError:
                    pass
            return parent
    return pathlib.Path.cwd().resolve()


def _default_path_to_project() -> dict[str, str]:
    return {
        v["path"]: v["manifest_project"]
        for v in constants.DEFAULT_ROLLERS.values()
    }


def parse_manual_roll_repos(fuchsia_root: pathlib.Path) -> dict[str, str]:
    """Parse `REPOS=([path]=manifest_project ...)` from //sdk/lib/c/manual-roll.sh."""
    manual_roll = fuchsia_root / "sdk" / "lib" / "c" / "manual-roll.sh"
    if manual_roll.exists():
        text = manual_roll.read_text(encoding="utf-8")
    else:
        text = utils.fetch_remote_gob_text(constants.MANUAL_ROLL_URL) or ""
    if not text:
        return _default_path_to_project()
    match = re.search(r"declare\s+-r\s+-A\s+REPOS=\((.*?)\)", text, re.DOTALL)
    if match is None:
        return _default_path_to_project()
    repos = {
        entry.group(1).strip(): entry.group(2).strip()
        for entry in re.finditer(r"\[([^\]]+)\]=([^\s\n]+)", match.group(1))
    }
    return repos if repos else _default_path_to_project()


def parse_luci_scheduler_configs(
    fuchsia_root: pathlib.Path,
) -> dict[str, dict[str, str]]:
    """Discover roller jobs and builders from generated luci-scheduler.cfg files."""
    gen_root = fuchsia_root / "integration" / "infra" / "config" / "generated"
    if not gen_root.exists():
        return {}

    repo_to_job: dict[str, tuple[str, str]] = {}
    jobs_meta: dict[tuple[str, str], dict[str, str]] = {}

    for cfg_path in sorted(gen_root.glob("*/luci/luci-scheduler.cfg")):
        luci_project = cfg_path.parent.parent.name
        try:
            content = cfg_path.read_text(encoding="utf-8")
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
            if jid_m is None:
                continue
            jid = jid_m.group(1)
            sched_m = re.search(
                r'^\s*schedule:\s*"([^"]+)"', block, re.MULTILINE
            )
            bb_m = re.search(r"buildbucket\s*\{(.*?)\}", block, re.DOTALL)
            if bb_m is None:
                continue
            bb_block = bb_m.group(1)
            bucket_m = re.search(r'bucket:\s*"([^"]+)"', bb_block)
            builder_m = re.search(r'builder:\s*"([^"]+)"', bb_block)
            if bucket_m is not None and builder_m is not None:
                bucket_name = bucket_m.group(1)
                builder_name = builder_m.group(1)
                jobs_meta[(luci_project, jid)] = {
                    "luci_project": luci_project,
                    "scheduler_job": jid,
                    "schedule": (
                        sched_m.group(1) if sched_m is not None else "triggered"
                    ),
                    "bucket": bucket_name,
                    "builder_name": builder_name,
                    "builder": f"{luci_project}/{bucket_name}/{builder_name}",
                }

    return {
        r_url: jobs_meta[key]
        for r_url, key in repo_to_job.items()
        if key in jobs_meta
    }


def resolve_live_scheduler_jobs(
    fuchsia_root: pathlib.Path,
    rollers: Mapping[str, MutableMapping[str, Any]],
) -> None:
    """Query live LUCI Scheduler `GetJobs` so generated hex suffixes stay fresh."""
    try:
        token = luci.get_luci_token(fuchsia_root)
    except (OSError, subprocess.SubprocessError, ValueError):
        return

    projects = {str(info["luci_project"]) for info in rollers.values()}
    live_jobs_by_project: dict[str, list[str]] = {}
    for proj in projects:
        try:
            data = luci.scheduler_rpc(token, "GetJobs", {"project": proj})
            live_jobs_by_project[proj] = [
                str(j["jobRef"]["job"])
                for j in data.get("jobs", ())
                if isinstance(j, dict)
                and isinstance(j.get("jobRef"), dict)
                and "job" in j["jobRef"]
            ]
        except (OSError, urllib.error.URLError, ValueError):
            continue

    for roller_name, info in rollers.items():
        proj = str(info["luci_project"])
        live_list = live_jobs_by_project.get(proj, [])
        if not live_list or info["scheduler_job"] in live_list:
            continue
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
                if bucket_match := any_bucket_pat.match(candidate):
                    info["bucket"] = bucket_match.group(1)
                    info["builder"] = f"{proj}/{info['bucket']}/{roller_name}"
                    info["scheduler_job"] = candidate
                    break


def discover_rollers_and_pending(
    fuchsia_root: pathlib.Path,
) -> tuple[dict[str, dict[str, Any]], dict[str, dict[str, Any]]]:
    """Dynamically discover roller configs and pending upstream commits."""
    manifest_b64 = utils.urlopen_bytes(constants.MANIFEST_URL, timeout=15)
    manifest_xml = base64.b64decode(manifest_b64).decode("utf-8")
    root = ET.fromstring(manifest_xml)

    path_to_proj = parse_manual_roll_repos(fuchsia_root)
    proj_to_path = {
        v["manifest_project"]: v["path"]
        for v in constants.DEFAULT_ROLLERS.values()
    } | {proj: path for path, proj in path_to_proj.items()}
    sched_by_remote = parse_luci_scheduler_configs(fuchsia_root)

    default_proj = parse_default_luci_project(fuchsia_root)
    fallback_by_proj: dict[str, tuple[str, dict[str, Any]]] = {
        v["manifest_project"]: (
            k,
            {
                **v,
                "luci_project": default_proj,
                "builder": f"{default_proj}/{v['bucket']}/{k}",
                "scheduler_job": f"{v['bucket']}-{k}",
            },
        )
        for k, v in constants.DEFAULT_ROLLERS.items()
    }

    raw_rollers: dict[str, dict[str, Any]] = {}
    raw_pending: dict[str, dict[str, Any]] = {}

    for proj_el in root.findall("./projects/project"):
        proj_name = proj_el.attrib.get("name")
        if proj_name is None or proj_name not in proj_to_path:
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
        raw_rollers[roller_name] = {
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
        raw = utils.urlopen_bytes(log_url, timeout=15).decode("utf-8")
        data = json.loads(utils.strip_json_xsrf_prefix(raw))
        commits = data.get("log", ())
        head_rev = commits[0]["commit"] if commits else rev
        parsed_commits: list[dict[str, Any]] = []
        commit_epochs: list[float] = []
        for commit in commits:
            t_str = commit.get("committer", {}).get("time", "")
            c_epoch: float | None = None
            if t_str:
                try:
                    c_epoch = datetime.datetime.strptime(
                        t_str, "%a %b %d %H:%M:%S %Y %z"
                    ).timestamp()
                    commit_epochs.append(c_epoch)
                except ValueError:
                    pass
            msg_lines = commit.get("message", "").splitlines()
            parsed_commits.append(
                {
                    "commit": commit["commit"],
                    "short_sha": commit["commit"][:12],
                    "subject": msg_lines[0] if msg_lines else "",
                    "author": commit.get("author", {}).get("email", ""),
                    "time": t_str,
                    "epoch": c_epoch,
                }
            )
        raw_pending[roller_name] = {
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

    ordered_names = tuple(
        k for k in constants.DEFAULT_ROLLERS if k in raw_rollers
    ) + tuple(k for k in raw_rollers if k not in constants.DEFAULT_ROLLERS)
    rollers = {k: raw_rollers[k] for k in ordered_names}
    pending = {k: raw_pending[k] for k in ordered_names}

    resolve_live_scheduler_jobs(fuchsia_root, rollers)
    return rollers, pending
