# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Flake auto-retrigger policy with persistent backoff, state persistence, and RPC triggers."""

import datetime
import json
import pathlib
import re
import subprocess
import sys
import urllib.error
import uuid
from collections.abc import Mapping, MutableMapping
from typing import IO, Any

from libc_rollers import constants, gerrit, luci, triage, utils


def evaluate_flake_retrigger_policy(
    fuchsia_root: pathlib.Path,
    roller_name: str,
    roller_data: Mapping[str, Any],
    all_reports: Mapping[str, Mapping[str, Any]],
    tree_info: Mapping[str, Any],
    state: Mapping[str, Any],
) -> dict[str, Any]:
    """Evaluate whether to retrigger immediately or back off on persistent flakes."""
    builds = roller_data.get("recent_builds", ())
    latest = builds[0] if builds else {}
    latest_id = latest.get("id", "unknown")
    latest_status = latest.get("status", "UNKNOWN")
    classification = roller_data.get("failure_classification") or {}
    flakes = classification.get("flakes", ())
    current_sigs = set(classification.get("flake_signatures", ()))

    # 1. Count consecutive runs of this roller with overlapping flake signatures.
    same_roller_streak = 1 if current_sigs else 0
    for older in builds[1:]:
        older_sigs = set(older.get("flake_signatures", ()))
        if older_sigs and (older_sigs & current_sigs):
            same_roller_streak += 1
        elif older.get("status") in ("SUCCESS", "FAILURE", "INFRA_FAILURE"):
            break

    # 2. Check if any sibling libc roller is hitting the same signature.
    cross_roller_matches: list[str] = []
    for other_name, other_data in all_reports.items():
        if other_name == roller_name:
            continue
        other_builds = other_data.get("recent_builds", ())
        other_latest = other_builds[0] if other_builds else {}
        if other_latest.get("status") not in (
            "FAILURE",
            "INFRA_FAILURE",
            "STARTED",
        ):
            continue
        other_cls = other_data.get("failure_classification") or {}
        other_sigs = set(other_cls.get("flake_signatures", ()))
        if current_sigs & other_sigs:
            cross_roller_matches.append(other_name)

    # 3. Check persisted streak state from monitor_state.json when the streak
    # fills the recent builds window.
    roller_streak_state = state.get("flake_streaks", {}).get(roller_name, {})
    prev_sigs = set(roller_streak_state.get("signatures", ()))
    prev_last_build = roller_streak_state.get("last_build_id")
    prev_roller_streak = int(
        roller_streak_state.get(
            "same_roller_streak", max(1, same_roller_streak)
        )
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

    since_dt = utils.parse_iso_utc(latest.get("createTime"))
    sorted_sigs = sorted(current_sigs)
    keywords = triage.extract_failure_keywords(sorted_sigs)
    gardener_info = gerrit.fetch_gardener_and_main_fixes(since_dt, keywords)
    ci_info = triage.check_ci_builders_for_flakes(
        fuchsia_root,
        str(roller_data.get("luci_project", "fuchsia")),
        flakes,
        sorted_sigs,
        since_dt,
    )

    is_persistent = (
        state_streak >= 2
        or bool(cross_roller_matches)
        or bool(ci_info["ci_persistently_failing"])
    )
    streak_count = max(
        state_streak + (len(cross_roller_matches) if state_streak >= 2 else 0),
        2 if is_persistent else 1,
    )

    # 4. Evaluate recovery / gardener fix signals since the failing roll started.
    fix_signals: list[str] = []
    for fix in gardener_info.get("matching_fixes_since_roll", ()):
        fix_signals.append(
            f"matching fix/revert on main ({fix['short_sha']} {fix['subject']})"
        )

    reopened_dt = utils.parse_iso_utc(tree_info.get("last_reopened_at"))
    if (
        reopened_dt is not None
        and since_dt is not None
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
                "affected global.ci builder(s) passed on a newer build "
                "after roll start"
            )
        elif streak_count >= 3 and ci_info["ci_two_green_after_roll"]:
            fix_signals.append(
                "affected global.ci builder(s) passed 2 consecutive builds "
                "after roll start"
            )

    likely_fixed = bool(fix_signals)

    if not is_persistent:
        cooldown_sec = 0
    elif ci_info["ci_still_failing"] and not gardener_info.get(
        "matching_fixes_since_roll"
    ):
        cooldown_sec = constants.MAX_PERSISTENT_BACKOFF_SECONDS
    else:
        cooldown_sec = constants.PERSISTENT_BACKOFF_SECONDS.get(
            streak_count, constants.MAX_PERSISTENT_BACKOFF_SECONDS
        )

    now = datetime.datetime.now(datetime.timezone.utc)
    ref_timestamps = (
        utils.parse_iso_utc(latest.get("endTime")),
        utils.parse_iso_utc(latest.get("createTime")),
        utils.parse_iso_utc(roller_streak_state.get("last_retrigger_at")),
    )
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
            f"persistent failure (streak={streak_count}); "
            f"waiting for STARTED build {latest_id} to finish"
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
            "corresponding global.ci builder latest completed build is still "
            "failing with the same signature"
        )

    return {
        "should_retrigger": should_retrigger,
        "already_retriggered": already_retriggered,
        "is_persistent": is_persistent,
        "same_roller_streak": state_streak,
        "streak_count": streak_count,
        "signatures": sorted_sigs,
        "cross_roller_matches": cross_roller_matches,
        "cooldown_seconds": cooldown_sec,
        "elapsed_seconds": elapsed_sec,
        "likely_fixed": likely_fixed,
        "fix_signals": fix_signals,
        "backoff_reasons": backoff_reasons,
        "tree": dict(tree_info),
        "ci_health": ci_info,
        "gardener_activity": gardener_info,
    }


def load_monitor_state(state_file: pathlib.Path) -> dict[str, Any]:
    """Load persistent flake-retrigger state, preserving caller-managed keys."""
    state: dict[str, Any] = {
        "retriggered_builds": {},
        "flake_streaks": {},
    }
    if state_file.exists():
        try:
            loaded = json.loads(state_file.read_text(encoding="utf-8"))
            if isinstance(loaded, dict):
                state.update(loaded)
        except (OSError, json.JSONDecodeError):
            pass
    return state


def save_monitor_state(
    state_file: pathlib.Path, state: Mapping[str, Any]
) -> None:
    """Atomically write persistent flake-retrigger state to disk."""
    utils.atomic_write_json(state_file, state)


def retrigger_roller(
    fuchsia_root: pathlib.Path,
    rollers: Mapping[str, MutableMapping[str, Any]],
    roller_name: str,
    reason: str,
) -> dict[str, Any]:
    """Emit a Buildbucket trigger to LUCI Scheduler for `roller_name`."""
    if roller_name not in rollers:
        raise ValueError(
            f"Unknown roller {roller_name!r}. "
            f"Discovered rollers: {list(rollers)}"
        )
    info = rollers[roller_name]
    if not re.search(r"-[0-9a-f]{6,}$", str(info["scheduler_job"])):
        luci.update_scheduler_job_from_builds(
            info,
            luci.fetch_recent_builds(
                fuchsia_root, str(info["builder"]), count=3
            ),
        )
    token = luci.get_luci_token(fuchsia_root)
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
    return luci.scheduler_rpc(token, "EmitTriggers", payload)


def run_monitor_actions(
    fuchsia_root: pathlib.Path,
    rollers: Mapping[str, MutableMapping[str, Any]],
    report: Mapping[str, Mapping[str, Any]],
    state: MutableMapping[str, Any],
    state_file: pathlib.Path,
    *,
    disable_retrigger: bool = False,
    log_stream: IO[str] | None = None,
) -> dict[str, list[dict[str, Any]]]:
    """Evaluate flake auto-retrigger policy and return structured issue records."""
    out = log_stream if log_stream is not None else sys.stdout
    now_iso = datetime.datetime.now(datetime.timezone.utc).isoformat()
    state_dirty = False
    summary: dict[str, list[dict[str, Any]]] = {
        "retriggered": [],
        "retrigger_errors": [],
        "held_flakes": [],
        "actionable_issues": [],
    }

    for name, data in report.items():
        builds = data.get("recent_builds", ())
        if not builds:
            print(f"[MONITOR] {name}: no builds found", file=out)
            continue
        latest = builds[0]
        latest_id = latest.get("id", "unknown")
        latest_status = latest.get("status", "UNKNOWN")
        if latest_status == "SUCCESS":
            if name in state.get("flake_streaks", {}):
                state["flake_streaks"].pop(name, None)
                state_dirty = True
            print(
                f"[MONITOR] {name}: latest build {latest_id} succeeded",
                file=out,
            )
            continue

        classification = data.get("failure_classification") or {}
        actionable = classification.get("actionable", ())
        flakes = classification.get("flakes", ())

        if not actionable and not flakes:
            print(
                f"[MONITOR] {name}: build {latest_id} is {latest_status} "
                "with 0 failures so far",
                file=out,
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

            if policy.get("should_retrigger") and not disable_retrigger:
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
                        f"Auto-retrigger after unrelated Fuchsia flake on "
                        f"{latest_id}{fix_note}",
                    )
                    state.setdefault("retriggered_builds", {})[name] = latest_id
                    streak_entry["last_retrigger_at"] = now_iso
                    save_monitor_state(state_file, state)
                    state_dirty = False
                    summary["retriggered"].append(
                        {
                            "roller": name,
                            "build_id": str(latest_id),
                            "flakes": flake_desc,
                        }
                    )
                    print(
                        f"[MONITOR] {name}: auto-retriggered after unrelated "
                        f"flake(s) [{flake_desc}] on build {latest_id}"
                        f"{fix_note}: {json.dumps(resp)}",
                        file=out,
                    )
                except (
                    OSError,
                    subprocess.SubprocessError,
                    urllib.error.URLError,
                    ValueError,
                ) as exc:
                    summary["retrigger_errors"].append(
                        {
                            "roller": name,
                            "build_id": str(latest_id),
                            "error": str(exc),
                        }
                    )
                    print(
                        f"[MONITOR] {name}: failed to auto-retrigger build "
                        f"{latest_id}: {exc}",
                        file=out,
                    )
            else:
                if policy.get("should_retrigger") and disable_retrigger:
                    reasons = ["retrigger disabled (--disable-retrigger)"]
                else:
                    reasons = list(
                        policy.get("backoff_reasons")
                        or ["holding off retrigger"]
                    )
                reason_text = "; ".join(reasons)
                summary["held_flakes"].append(
                    {
                        "roller": name,
                        "build_id": str(latest_id),
                        "reasons": reasons,
                    }
                )
                print(
                    f"[MONITOR] {name}: holding off retrigger on build "
                    f"{latest_id} for unrelated flake(s) [{flake_desc}] -- "
                    f"{reason_text}",
                    file=out,
                )
            continue

        if latest_status in ("STARTED", "SCHEDULED"):
            print(
                f"[MONITOR] {name}: build {latest_id} is still {latest_status} "
                f"with {len(actionable)} candidate actionable failure(s); "
                "waiting for build completion",
                file=out,
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
        raw_issue_key = (
            classification.get("issue_key") or f"{safe_name}:{safe_latest_id}"
        )
        safe_issue_key = (
            str(raw_issue_key)
            if re.fullmatch(r"[A-Za-z0-9_./:-]{1,160}", str(raw_issue_key))
            else f"{safe_name}:{safe_latest_id}"
        )
        raw_issue_title = (
            classification.get("issue_title")
            or f"[{safe_name}] Issue on build {safe_latest_id}"
        )
        safe_issue_title = (
            str(raw_issue_title)
            if re.fullmatch(
                r"[A-Za-z0-9_./:() \[\]-]{1,200}", str(raw_issue_title)
            )
            else f"[{safe_name}] Issue on build {safe_latest_id}"
        )

        raw_cl_url = str(data.get("active_or_failed_cl") or "")
        safe_cl_url = (
            raw_cl_url
            if re.fullmatch(
                r"https://fuchsia-review\.googlesource\.com/c/fuchsia/\+/[0-9]+(?:/[0-9]+)?",
                raw_cl_url,
            )
            else ""
        )
        m_cl_num = re.search(r"/\+/([0-9]+)", safe_cl_url)
        safe_cl_num = m_cl_num.group(1) if m_cl_num is not None else ""
        lf = data.get("last_failed_run") or {}
        lf_bid = str(lf.get("build_id") or "")
        safe_lf_url = (
            f"{constants.BUILDBUCKET_BUILD_BASE_URL}/{lf_bid}"
            if re.fullmatch(r"[0-9]+", lf_bid)
            else ""
        )
        p_info = data.get("pending") or {}
        pending_commits = p_info.get("pending_commits", ())
        safe_shas = [
            str(c.get("short_sha", ""))
            for c in pending_commits[:15]
            if re.fullmatch(r"[0-9a-f]{7,40}", str(c.get("short_sha", "")))
        ]
        raw_pinned = str(p_info.get("pinned_revision", ""))
        safe_pinned = (
            raw_pinned if re.fullmatch(r"[0-9a-f]{7,40}", raw_pinned) else ""
        )
        raw_head = str(p_info.get("upstream_head", ""))
        safe_head = (
            raw_head if re.fullmatch(r"[0-9a-f]{7,40}", raw_head) else ""
        )
        failing_builders: list[dict[str, str]] = []
        for failure in actionable[:10]:
            f_builder = str(failure.get("builder", ""))
            f_bid = str(failure.get("build_id", ""))
            if re.fullmatch(r"[A-Za-z0-9_.-]+", f_builder) and re.fullmatch(
                r"[0-9]+", f_bid
            ):
                failing_builders.append(
                    {
                        "builder": f_builder,
                        "build_id": f_bid,
                        "build_url": (
                            f"{constants.BUILDBUCKET_BUILD_BASE_URL}/{f_bid}"
                        ),
                    }
                )
        roller_build_url = (
            f"{constants.BUILDBUCKET_BUILD_BASE_URL}/{safe_latest_id}"
        )
        raw_remote = str(data.get("remote", ""))
        safe_remote = (
            raw_remote
            if re.fullmatch(r"https://[A-Za-z0-9_./-]+", raw_remote)
            else ""
        )
        raw_path = str(data.get("path", ""))
        safe_path = (
            raw_path if re.fullmatch(r"[A-Za-z0-9_./-]+", raw_path) else ""
        )
        safe_proj = (
            str(data.get("luci_project", ""))
            if re.fullmatch(r"[a-z0-9_-]+", str(data.get("luci_project", "")))
            else "fuchsia"
        )
        safe_bucket = (
            str(data.get("bucket", ""))
            if re.fullmatch(r"[A-Za-z0-9_.-]+", str(data.get("bucket", "")))
            else "global.roller"
        )
        safe_job = (
            str(data.get("scheduler_job", ""))
            if re.fullmatch(
                r"[A-Za-z0-9_.-]+", str(data.get("scheduler_job", ""))
            )
            else ""
        )

        summary["actionable_issues"].append(
            {
                "roller": safe_name,
                "luci_project": safe_proj,
                "bucket": safe_bucket,
                "builder": f"{safe_proj}/{safe_bucket}/{safe_name}",
                "scheduler_job": safe_job,
                "issue_key": safe_issue_key,
                "issue_title": safe_issue_title,
                "roller_build_id": safe_latest_id,
                "roller_build_url": roller_build_url,
                "roller_status": safe_status,
                "roll_cl_num": safe_cl_num,
                "roll_cl_url": safe_cl_url,
                "last_failed_url": safe_lf_url,
                "failing_builders": failing_builders,
                "pending_shas": safe_shas,
                "pending_count": int(
                    p_info.get("pending_count", len(pending_commits))
                ),
                "pinned_revision": safe_pinned,
                "upstream_head": safe_head,
                "upstream_remote": safe_remote,
                "target_path": safe_path,
            }
        )
        print(
            f"[MONITOR] {safe_name}: actionable failure '{safe_issue_title}' "
            f"on build {safe_latest_id}",
            file=out,
        )

    if state_dirty:
        save_monitor_state(state_file, state)
    return summary
