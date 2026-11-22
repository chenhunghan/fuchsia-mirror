# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Gerrit REST and Gitiles commit queries for Fuchsia libc rollers."""

import datetime
import json
import pathlib
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request
from collections.abc import Collection, Mapping, MutableMapping, Sequence
from typing import Any

from libc_rollers import constants, luci, utils


def format_cl_url(
    cl_num: str | None, patchset: str | int | None = None
) -> str | None:
    """Format a canonical Fuchsia Gerrit CL URL with optional patchset."""
    if not cl_num:
        return None
    base = f"{constants.GERRIT_BASE_URL}/c/fuchsia/+/{cl_num}"
    if patchset is not None and str(patchset) != "":
        return f"{base}/{patchset}"
    return base


def extract_gerrit_url_from_summary(summary_md: str) -> str | None:
    """Extract the fallback Gerrit URL from a roller build's summaryMarkdown."""
    if match := re.search(
        r"https://fuchsia-review\.googlesource\.com/[^\s\)]+",
        summary_md or "",
    ):
        return match.group(0)
    return None


def count_cq_build_statuses(
    builds: Sequence[Mapping[str, Any]],
) -> dict[str, int]:
    """Count total, passing, failing/canceled, and running Buildbucket builds."""
    total = len(builds)
    passed = sum(1 for b in builds if b.get("status") == "SUCCESS")
    failed = sum(
        1
        for b in builds
        if b.get("status") in ("FAILURE", "INFRA_FAILURE", "CANCELED")
    )
    running = sum(
        1 for b in builds if b.get("status") in ("STARTED", "SCHEDULED")
    )
    return {
        "total": total,
        "passed": passed,
        "failed": failed,
        "running": running,
    }


def _gerrit_get_json(path_and_query: str, timeout: int = 10) -> Any:
    """Perform an unauthenticated GET against public Fuchsia Gerrit JSON API."""
    url = f"{constants.GERRIT_BASE_URL}{path_and_query}"
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        raw = resp.read().decode("utf-8")
    return json.loads(utils.strip_json_xsrf_prefix(raw))


def fetch_recent_merged_rolls(
    manifest_project: str, limit: int = 15
) -> list[dict[str, Any]]:
    """Fetch recent merged roll CLs from Gerrit with convergence latency and attempts."""
    rolls: list[dict[str, Any]] = []
    try:
        query = urllib.parse.quote(
            f'project:fuchsia status:merged message:"[roll] Roll {manifest_project}"'
        )
        entries = _gerrit_get_json(
            f"/changes/?q={query}&n={limit}&o=CURRENT_REVISION", timeout=10
        )
        if not isinstance(entries, list):
            return rolls
        for entry in entries:
            subj = entry.get("subject", "")
            if re.search(r"^(?:Revert\b|\[revert\])", subj, re.IGNORECASE):
                continue
            cl_num = str(entry.get("_number", ""))
            created_ep = utils.gerrit_date_to_epoch(entry.get("created"))
            submitted_ep = utils.gerrit_date_to_epoch(
                entry.get("submitted") or entry.get("updated")
            )
            if not cl_num or created_ep is None or submitted_ep is None:
                continue
            conv_h = round(max(0.1, (submitted_ep - created_ep) / 3600.0), 2)
            # Gerrit creates a final rebase patchset on submit, so PS2 = 1
            # attempt, PS3 = 2 attempts, etc.
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
    except (OSError, urllib.error.URLError, ValueError):
        # Best-effort historical SLO enrichment; keep status reporting live if Gerrit is unreachable.
        pass
    return rolls


def fetch_gardener_and_main_fixes(
    since_dt: datetime.datetime | None, keywords: Collection[str]
) -> dict[str, list[dict[str, Any]]]:
    """Inspect recent fuchsia.git main commits for gardener reverts or fixes."""
    reverts_since_roll: list[dict[str, Any]] = []
    matching_fixes: list[dict[str, Any]] = []
    try:
        url = (
            "https://fuchsia.googlesource.com/fuchsia/"
            "+log/refs/heads/main?n=40&format=JSON"
        )
        raw = utils.urlopen_bytes(url, timeout=15).decode("utf-8")
        commits = json.loads(utils.strip_json_xsrf_prefix(raw)).get("log", ())
        for commit in commits:
            t_str = commit.get("committer", {}).get("time", "")
            try:
                c_dt = datetime.datetime.strptime(
                    t_str, "%a %b %d %H:%M:%S %Y %z"
                ).astimezone(datetime.timezone.utc)
            except ValueError:
                continue
            if since_dt is not None and c_dt <= since_dt:
                continue
            msg = commit.get("message", "")
            subj = msg.splitlines()[0] if msg else ""
            if re.search(r"^(?:Reland\b|\[reland\])", subj, re.IGNORECASE):
                continue
            msg_lower = msg.lower()
            entry = {
                "short_sha": commit.get("commit", "")[:12],
                "subject": subj,
                "time": c_dt.isoformat(),
                "message_lower": msg_lower,
            }
            is_revert = bool(
                re.search(r"^(?:Revert\b|\[revert\])", subj, re.IGNORECASE)
            )
            if is_revert:
                reverts_since_roll.append(entry)
            if keywords and any(kw in msg_lower for kw in keywords):
                if is_revert or re.search(
                    r"\b(?:fix|disable|flake|flaky|skip|workaround|revert)\b",
                    msg_lower,
                ):
                    matching_fixes.append(
                        {
                            "short_sha": entry["short_sha"],
                            "subject": entry["subject"],
                            "time": entry["time"],
                        }
                    )
    except (OSError, ValueError, urllib.error.URLError, RuntimeError):
        pass
    return {
        "reverts_since_roll": reverts_since_roll,
        "matching_fixes_since_roll": matching_fixes,
    }


def fetch_watched_cl_status(
    fuchsia_root: pathlib.Path, cl_num: str
) -> dict[str, Any] | None:
    """Fetch live Gerrit and CQ status for an optionally watched CL."""
    if not re.fullmatch(r"[0-9]+", str(cl_num)):
        return None
    try:
        data = _gerrit_get_json(
            f"/changes/{cl_num}?o=CURRENT_REVISION&o=LABELS&o=DETAILED_ACCOUNTS",
            timeout=10,
        )
    except (OSError, urllib.error.URLError, ValueError):
        return None
    if not isinstance(data, dict):
        return None

    status = data.get("status", "UNKNOWN")
    ps_num = data.get("current_revision_number")
    curr_rev = data.get("current_revision", "")
    rev_info = data.get("revisions", {}).get(curr_rev, {})
    ps_created = utils.gerrit_date_to_epoch(
        rev_info.get("created") or data.get("updated")
    )
    updated_epoch = utils.gerrit_date_to_epoch(data.get("updated"))

    labels_raw = data.get("labels", {})
    labels_summary: dict[str, int] = {}
    for label_name in (
        "Code-Review",
        "Presubmit-Verified",
        "Commit-Queue",
        "Fuchsia-Auto-Submit",
    ):
        lobj = labels_raw.get(label_name, {})
        if "approved" in lobj:
            val = 2
        elif "recommended" in lobj:
            val = 1
        elif "rejected" in lobj:
            val = -2
        elif "disliked" in lobj:
            val = -1
        elif "value" in lobj:
            val = int(lobj.get("value") or 0)
        else:
            val = 0
        labels_summary[label_name] = val

    cq_counts: dict[str, int] | None = None
    if ps_num and status == "NEW":
        try:
            _, builds = luci.fetch_cl_builds(
                fuchsia_root, str(cl_num), str(ps_num)
            )
            if builds:
                cq_counts = count_cq_build_statuses(builds)
        except (OSError, subprocess.SubprocessError):
            pass

    return {
        "cl_num": str(cl_num),
        "patchset": ps_num,
        "cl_url": format_cl_url(str(cl_num), ps_num),
        "subject": data.get("subject", f"fxr/{cl_num}"),
        "status": status,
        "ps_created_epoch": ps_created,
        "updated_epoch": updated_epoch,
        "unresolved_comment_count": data.get("unresolved_comment_count", 0),
        "total_comment_count": data.get("total_comment_count", 0),
        "labels": labels_summary,
        "cq": cq_counts,
    }


def fetch_last_landed_roll(
    fuchsia_root: pathlib.Path,
    roller_name: str,
    manifest_project: str,
    pinned_rev: str,
    cache: MutableMapping[tuple[str, str], dict[str, Any]] | None = None,
) -> dict[str, Any]:
    """Resolve when the current pinned revision landed in fuchsia.git."""
    cache_key = (roller_name, pinned_rev)
    if cache is not None and cache_key in cache:
        return cache[cache_key]

    try:
        query = urllib.parse.quote(
            f'project:fuchsia status:merged message:"[roll] Roll {manifest_project}"'
        )
        entries = _gerrit_get_json(
            f"/changes/?q={query}&n=1&o=CURRENT_REVISION", timeout=10
        )
        if isinstance(entries, list) and entries:
            entry = entries[0]
            cl_num = str(entry.get("_number", "")) or None
            epoch = utils.gerrit_date_to_epoch(
                entry.get("submitted") or entry.get("updated")
            )
            info: dict[str, Any] = {
                "commit": (entry.get("current_revision") or "")[:12] or None,
                "epoch": epoch,
                "subject": entry.get("subject", ""),
                "cl_num": cl_num,
                "cl_url": format_cl_url(cl_num),
            }
            if cache is not None:
                cache[cache_key] = info
            return info
    except (OSError, urllib.error.URLError, ValueError):
        pass

    if (fuchsia_root / ".git").exists():
        try:
            with subprocess.Popen(
                (
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
                ),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
            ) as proc:
                assert proc.stdout is not None
                out = proc.stdout.read().strip()
                rc = proc.wait(timeout=15)
            if rc == 0 and out and "|" in out:
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
                git_info: dict[str, Any] = {
                    "commit": commit_sha,
                    "epoch": epoch,
                    "subject": first_line,
                    "cl_num": cl_num,
                    "cl_url": format_cl_url(cl_num),
                }
                if cache is not None:
                    cache[cache_key] = git_info
                return git_info
        except (OSError, subprocess.SubprocessError, ValueError):
            pass

    return {
        "commit": None,
        "epoch": None,
        "subject": None,
        "cl_num": None,
        "cl_url": None,
    }
