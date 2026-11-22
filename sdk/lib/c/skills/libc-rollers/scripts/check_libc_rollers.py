#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Status, triage, and retrigger CLI tool for Fuchsia libc rollers.

Dynamically discovers the LLVM-based libc repositories from
//sdk/lib/c/manual-roll.sh alongside companion repositories maintained in
concert with libc (such as linux-syscall-support, used by elfldltl and the
dynamic linker), and resolves their current Buildbucket builders and LUCI
Scheduler job IDs on the fly from roller names via Buildbucket build tags, live
LUCI Scheduler APIs, and //integration/infra/config.
"""

import argparse
import json
import os
import pathlib
import subprocess
import sys
import time
import urllib.error
from collections.abc import MutableMapping, Sequence
from typing import IO, Any

from libc_rollers import (
    config,
    constants,
    formatting,
    gerrit,
    luci,
    metrics,
    monitor,
    triage,
    utils,
)


def collect_rollers_snapshot(
    fuchsia_root: pathlib.Path,
    state_path: pathlib.Path,
    history_path: pathlib.Path,
    *,
    worktree_dir: pathlib.Path | None = None,
    watched_cl_nums: Sequence[str] | None = None,
    landed_cache: MutableMapping[tuple[str, str], dict[str, Any]] | None = None,
    run_monitor: bool = False,
    update_skill: bool = False,
    disable_retrigger: bool = False,
    log_stream: IO[str] | None = None,
) -> tuple[dict[str, dict[str, Any]], dict[str, Any], list[str]]:
    """Collect full roller report, summary snapshot dict, and skill sync notes."""
    rollers, pending = config.discover_rollers_and_pending(fuchsia_root)
    monitor_state = monitor.load_monitor_state(state_path)
    health_history = metrics.load_health_history(history_path)
    tree_info = luci.fetch_tree_status(fuchsia_root)
    now_epoch = time.time()

    report: dict[str, dict[str, Any]] = {}
    roller_rows: list[dict[str, Any]] = []

    for name, info in rollers.items():
        builds = luci.fetch_recent_builds(
            fuchsia_root, str(info["builder"]), count=3
        )
        luci.update_scheduler_job_from_builds(info, builds)

        step_cache: dict[str, dict[str, Any]] = {}
        cl_builds_cache: dict[tuple[str, str | None], list[dict[str, Any]]] = {}
        subbuild_cache: dict[str, list[dict[str, Any]]] = {}
        cls_cache: dict[str, dict[str, Any]] = {}
        roller_proj = str(info.get("luci_project", "fuchsia"))
        for build in builds:
            bid = build.get("id")
            if bid and build.get("status") in (
                "STARTED",
                "FAILURE",
                "INFRA_FAILURE",
            ):
                step_cache[str(bid)] = luci.inspect_roll_build_steps(
                    fuchsia_root, str(bid)
                )

        for idx, build in enumerate(builds):
            bid_str = str(build.get("id") or "")
            if bid_str in step_cache:
                prev_info: dict[str, Any] | None = None
                for older in builds[idx + 1 :]:
                    older_id = str(older.get("id") or "")
                    if older_id in step_cache:
                        prev_info = step_cache[older_id]
                        break
                build["attempt_kind"] = formatting.describe_roll_attempt(
                    step_cache[bid_str], prev_info
                )
                b_cl = step_cache[bid_str].get("cl_num")
                b_ps = step_cache[bid_str].get("patchset")
                if b_cl:
                    cache_key = (str(b_cl), str(b_ps) if b_ps else None)
                    if cache_key not in cl_builds_cache:
                        try:
                            _, fetched_builds = luci.fetch_cl_builds(
                                fuchsia_root, cache_key[0], cache_key[1]
                            )
                            cl_builds_cache[cache_key] = fetched_builds
                        except (OSError, subprocess.SubprocessError):
                            cl_builds_cache[cache_key] = []
                    b_cls = triage.classify_roll_failures(
                        name,
                        build,
                        step_cache[bid_str],
                        cl_builds_cache[cache_key],
                        fuchsia_root=fuchsia_root,
                        subbuild_cache=subbuild_cache,
                        luci_project=roller_proj,
                    )
                    cls_cache[bid_str] = b_cls
                    build["flake_signatures"] = b_cls.get(
                        "flake_signatures", []
                    )

        latest = builds[0] if builds else {}
        latest_id = str(latest.get("id") or "")
        latest_status = latest.get("status", "UNKNOWN")
        latest_steps = step_cache.get(latest_id, {})
        cl_num = latest_steps.get("cl_num")
        patchset = latest_steps.get("patchset")
        triage_md: str | None = None
        cl_builds: list[dict[str, Any]] = []
        if cl_num and latest_status in (
            "STARTED",
            "FAILURE",
            "INFRA_FAILURE",
        ):
            cache_key = (str(cl_num), str(patchset) if patchset else None)
            cl_err: str | None = None
            if cache_key in cl_builds_cache:
                cl_builds = cl_builds_cache[cache_key]
            else:
                try:
                    _, cl_builds = luci.fetch_cl_builds(
                        fuchsia_root, cache_key[0], cache_key[1]
                    )
                except (OSError, subprocess.SubprocessError) as exc:
                    cl_err = str(exc)
            if cl_err is not None:
                triage_md = cl_err
            else:
                triage_md, _ = triage.run_triage_cl(
                    fuchsia_root,
                    str(cl_num),
                    str(patchset) if patchset else None,
                    builds=cl_builds,
                    subbuild_cache=subbuild_cache,
                )

        failure_classification: dict[str, Any] | None = None
        if latest_status in ("STARTED", "FAILURE", "INFRA_FAILURE"):
            failure_classification = (
                cls_cache[latest_id]
                if latest_id in cls_cache
                else triage.classify_roll_failures(
                    name,
                    latest,
                    latest_steps,
                    cl_builds,
                    fuchsia_root=fuchsia_root,
                    subbuild_cache=subbuild_cache,
                    luci_project=roller_proj,
                )
            )

        last_failed = luci.fetch_last_failed_build(
            fuchsia_root, str(info["builder"]), builds
        )
        last_failed_info: dict[str, Any] | None = None
        last_failed_attempt: dict[str, Any] | None = None
        if last_failed is not None:
            f_id = str(last_failed.get("id") or "")
            f_steps = step_cache.get(f_id) or luci.inspect_roll_build_steps(
                fuchsia_root, f_id
            )
            f_cl = f_steps.get("cl_num")
            f_ps = f_steps.get("patchset")
            f_prev_steps: dict[str, Any] | None = None
            for idx, build in enumerate(builds):
                if str(build.get("id") or "") == f_id:
                    for older in builds[idx + 1 :]:
                        older_id = str(older.get("id") or "")
                        if older_id in step_cache:
                            f_prev_steps = step_cache[older_id]
                            break
                    break
            f_attempt_kind = formatting.describe_roll_attempt(
                f_steps, f_prev_steps
            )
            f_gerrit_url = (
                gerrit.format_cl_url(str(f_cl), f_ps)
                if f_cl
                else gerrit.extract_gerrit_url_from_summary(
                    last_failed.get("summaryMarkdown", "")
                )
            )
            f_triage_md: str | None = None
            f_brief = triage.first_meaningful_summary_line(
                last_failed.get("summaryMarkdown", "")
            )
            f_flake_sigs: list[str] = []
            if f_cl:
                f_key = (str(f_cl), str(f_ps) if f_ps else None)
                f_cl_builds = cl_builds_cache.get(f_key)
                if f_cl_builds is None:
                    try:
                        _, f_cl_builds = luci.fetch_cl_builds(
                            fuchsia_root, f_key[0], f_key[1]
                        )
                    except (OSError, subprocess.SubprocessError):
                        f_cl_builds = []
                f_triage_md, f_brief = triage.run_triage_cl(
                    fuchsia_root,
                    f_key[0],
                    f_key[1],
                    builds=f_cl_builds,
                    subbuild_cache=subbuild_cache,
                )
                f_cls = (
                    cls_cache[f_id]
                    if f_id in cls_cache
                    else triage.classify_roll_failures(
                        name,
                        last_failed,
                        f_steps,
                        f_cl_builds,
                        fuchsia_root=fuchsia_root,
                        subbuild_cache=subbuild_cache,
                        luci_project=roller_proj,
                    )
                )
                f_flake_sigs = f_cls.get("flake_signatures", [])
            elif f_gerrit_url:
                f_brief = f"{f_gerrit_url} ({f_brief})"
            f_build_url = f"{constants.BUILDBUCKET_BUILD_BASE_URL}/{f_id}"
            last_failed_info = {
                "build_id": f_id,
                "build_url": f_build_url,
                "status": last_failed.get("status"),
                "create_time": last_failed.get("createTime", ""),
                "end_time": last_failed.get("endTime", ""),
                "gerrit_url": f_gerrit_url,
                "attempt_kind": f_attempt_kind,
                "brief_summary": f_brief,
                "triage_report": f_triage_md,
                "flake_signatures": f_flake_sigs,
            }
            last_failed_attempt = {
                "build_id": f_id,
                "build_url": f_build_url,
                "start_epoch": utils.iso_to_epoch(
                    last_failed.get("createTime")
                ),
                "end_epoch": utils.iso_to_epoch(
                    last_failed.get("endTime") or last_failed.get("createTime")
                ),
                "cl_num": f_cl,
                "patchset": f_ps,
                "cl_url": f_gerrit_url,
                "attempt_kind": f_attempt_kind,
                "failure_summary": f_brief,
            }

        pending_attempt: dict[str, Any] | None = None
        if latest_status == "STARTED":
            cl_url = gerrit.format_cl_url(
                str(cl_num) if cl_num else None, patchset
            )
            cq_summary = (
                triage.summarize_cq_builds(
                    fuchsia_root,
                    str(cl_num),
                    str(patchset) if patchset else None,
                    cl_builds,
                )
                if (cl_num and cl_builds)
                else None
            )
            pending_attempt = {
                "build_id": latest_id or None,
                "build_url": (
                    f"{constants.BUILDBUCKET_BUILD_BASE_URL}/{latest_id}"
                    if latest_id
                    else None
                ),
                "status": "STARTED",
                "start_epoch": utils.iso_to_epoch(latest.get("createTime")),
                "cl_num": cl_num,
                "patchset": patchset,
                "cl_url": cl_url,
                "attempt_kind": latest.get("attempt_kind") or "in progress",
                "cq": cq_summary,
            }

        p_info = pending.get(name, {})
        pinned_rev = str(p_info.get("pinned_revision", ""))
        head_rev = str(p_info.get("upstream_head", ""))
        pending_count = int(p_info.get("pending_count", 0))
        pending_commits = [
            {
                "sha": c["short_sha"],
                "subject": c["subject"],
                "epoch": c.get("epoch"),
            }
            for c in p_info.get("pending_commits", ())
        ]

        last_landed = gerrit.fetch_last_landed_roll(
            fuchsia_root,
            name,
            str(info["manifest_project"]),
            pinned_rev,
            cache=landed_cache,
        )

        r_state = monitor_state.get("flake_streaks", {}).get(name, {})
        backoff_info = (
            {
                "streak": r_state.get("streak_count", 0),
                "signatures": r_state.get("signatures", []),
            }
            if r_state.get("streak_count", 0) >= 2
            else None
        )

        slo_eval = metrics.update_and_evaluate_roller_slo(
            history=health_history,
            roller_name=name,
            manifest_project=str(info["manifest_project"]),
            schedule=str(info["schedule"]),
            p_info=p_info,
            last_landed=last_landed,
            recent_builds=builds,
            failure_classification=failure_classification,
            pending_attempt=pending_attempt,
            now_epoch=now_epoch,
        )

        proj = str(info.get("luci_project", "fuchsia"))
        bucket = str(info.get("bucket", "global.roller"))
        job_id = str(info.get("scheduler_job", ""))

        roller_rows.append(
            {
                "name": name,
                "manifest_project": info["manifest_project"],
                "path": info["path"],
                "schedule": info["schedule"],
                "builder_url": (
                    f"https://ci.chromium.org/p/{proj}/builders/{bucket}/{name}"
                ),
                "scheduler_url": (
                    f"https://luci-scheduler.appspot.com/jobs/{proj}/{job_id}"
                ),
                "pinned_revision": pinned_rev[:12],
                "head_revision": head_rev[:12],
                "pending_count": pending_count,
                "pending_commits": pending_commits,
                "last_landed": last_landed,
                "pending_attempt": pending_attempt,
                "last_failed_attempt": last_failed_attempt,
                "backoff": backoff_info,
                "slo": slo_eval,
            }
        )

        report[name] = {
            **info,
            "pending": p_info,
            "last_landed": last_landed,
            "slo": slo_eval,
            "recent_builds": builds,
            "active_or_failed_cl": gerrit.format_cl_url(
                str(cl_num) if cl_num else None, patchset
            ),
            "active_attempt_kind": latest.get("attempt_kind"),
            "triage_report": triage_md,
            "failure_classification": failure_classification,
            "last_failed_run": last_failed_info,
        }

    try:
        metrics.save_health_history(history_path, health_history)
    except OSError:
        pass

    for name, data in report.items():
        cls = data.get("failure_classification") or {}
        if not cls.get("actionable") and cls.get("flakes"):
            data["retrigger_policy"] = monitor.evaluate_flake_retrigger_policy(
                fuchsia_root,
                name,
                data,
                report,
                tree_info,
                monitor_state,
            )
        else:
            data["retrigger_policy"] = None

    skill_updates = formatting.sync_skill_files(
        rollers, force_write=update_skill, worktree_dir=worktree_dir
    )

    monitor_summary = None
    if run_monitor:
        monitor_summary = monitor.run_monitor_actions(
            fuchsia_root,
            rollers,
            report,
            monitor_state,
            state_path,
            disable_retrigger=disable_retrigger,
            log_stream=log_stream,
        )

    watched_cls: list[dict[str, Any]] = []
    for cl_id in watched_cl_nums or ():
        cl_status = gerrit.fetch_watched_cl_status(fuchsia_root, cl_id)
        if cl_status is not None:
            watched_cls.append(cl_status)

    markdown_table = formatting.render_markdown_status_table(
        roller_rows, watched_cls=watched_cls, now_sec=now_epoch
    )
    snapshot = {
        "updated_epoch": now_epoch,
        "tree_status": {
            "tree": tree_info.get("tree", "fuchsia-stem"),
            "state": tree_info.get("general_state", "UNKNOWN"),
            "message": tree_info.get("message", ""),
            "epoch": utils.iso_to_epoch(tree_info.get("create_time")),
            "last_reopened_epoch": utils.iso_to_epoch(
                tree_info.get("last_reopened_at")
            ),
        },
        "rollers": roller_rows,
        "watched_cls": watched_cls,
        "skill_cl": watched_cls[0] if watched_cls else None,
        "markdown_table": markdown_table,
        "monitor_summary": monitor_summary,
    }
    return report, snapshot, skill_updates


def _build_argument_parser() -> argparse.ArgumentParser:
    default_state_dir = (
        pathlib.Path(
            os.environ.get(
                "XDG_STATE_HOME",
                str(pathlib.Path.home() / ".local" / "state"),
            )
        )
        / "fuchsia-libc-rollers"
    )
    parser = argparse.ArgumentParser(
        description=(
            "Pure CLI status, triage, flake retrigger, and SLO calibration "
            "tool for Fuchsia libc rollers."
        ),
        epilog=constants.JSON_SCHEMA_EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--fuchsia-dir",
        type=pathlib.Path,
        help=(
            "Optional path to a Fuchsia checkout (auto-detected if present; "
            "runs checkout-free via Gitiles if absent)."
        ),
    )
    parser.add_argument(
        "--worktree-dir",
        type=pathlib.Path,
        help=(
            "Optional writable Fuchsia worktree path targeted when "
            "--update-skill writes drifted config."
        ),
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help=(
            "Output structured status report (and --monitor summary when "
            "enabled) as JSON."
        ),
    )
    parser.add_argument(
        "--table-only",
        action="store_true",
        help="Output only the concise 4-column Markdown status table.",
    )
    parser.add_argument(
        "--retrigger",
        action="append",
        help=(
            "Manually retrigger the specified roller (e.g., llvm-libc-roller, "
            "scudo-roller, gwp_asan-roller, linux-syscall-support-roller)."
        ),
    )
    parser.add_argument(
        "--reason",
        default="Retrigger after unrelated Fuchsia flake",
        help="Reason string recorded when retriggering.",
    )
    parser.add_argument(
        "--update-skill",
        action="store_true",
        help=(
            "Self-update SKILL.md and constants.py DEFAULT_ROLLERS if any "
            "LUCI builder, bucket, URL, or schedule details changed."
        ),
    )
    parser.add_argument(
        "--monitor",
        action="store_true",
        help=(
            "Evaluate automated flake retrigger policy across all rollers "
            "(auto-retriggering transient unrelated flakes with "
            "persistent-failure backoff in --state-file) and populate "
            "monitor_summary in --json output."
        ),
    )
    parser.add_argument(
        "--disable-retrigger",
        action="store_true",
        help=(
            "Evaluate --monitor flake policy without sending LUCI Scheduler "
            "EmitTriggers RPCs."
        ),
    )
    parser.add_argument(
        "--watch-cl",
        action="append",
        default=[],
        help=(
            "Optional Gerrit CL number(s) to include in the status table and "
            "JSON snapshot."
        ),
    )
    parser.add_argument(
        "--state-file",
        type=pathlib.Path,
        default=default_state_dir / "monitor_state.json",
        help=(
            "Path to persistent JSON file tracking flake retrigger streaks "
            "for --monitor."
        ),
    )
    parser.add_argument(
        "--history-file",
        type=pathlib.Path,
        default=default_state_dir / "roller_health_history.json",
        help=(
            "Path to persistent JSON file tracking historical roll convergence "
            "and SLO thresholds."
        ),
    )
    parser.add_argument(
        "--bb-bin",
        type=pathlib.Path,
        help="Explicit path to the Buildbucket (bb) CLI binary (or set BB_BIN).",
    )
    parser.add_argument(
        "--luci-auth-bin",
        type=pathlib.Path,
        help=(
            "Explicit path to the luci-auth CLI binary (optional when "
            "LUCI_AUTH_CREDENTIAL_HELPER is set)."
        ),
    )
    parser.add_argument(
        "--luci-project",
        help=(
            "Explicit LUCI project owning global.roller builders "
            "(auto-discovered from //sdk/lib/c/README.md if omitted)."
        ),
    )
    parser.add_argument(
        "--set-slo",
        action="append",
        default=[],
        metavar="ROLLER=TARGET_H:BREACH_H",
        help=(
            "Update per-roller SLO thresholds in --history-file (e.g., "
            "--set-slo llvm-libc-roller=12:24 --set-slo scudo-roller=18:36)."
        ),
    )
    return parser


def _print_human_report(
    report: dict[str, dict[str, Any]],
    snapshot: dict[str, Any],
    skill_updates: Sequence[str],
) -> None:
    """Format and print the detailed human-readable roller triage report."""
    lines: list[str] = [f"[SKILL SYNC] {msg}" for msg in skill_updates]
    t_info = snapshot["tree_status"]
    lines.append(
        f"=== Tree Status ({t_info['tree']}): {t_info['state']} "
        f"({t_info['message']}) ==="
    )
    lines.append("")
    lines.append(snapshot["markdown_table"])
    lines.append("")

    for name, data in report.items():
        p_info = data["pending"]
        pinned_short = p_info.get("pinned_revision", "unknown")[:12]
        head_short = p_info.get("upstream_head", "unknown")[:12]
        p_count = p_info.get("pending_count", 0)
        lines.append(
            f"=== {name} ({data['manifest_project']}) | "
            f"job: {data['luci_project']}/{data['scheduler_job']} | "
            f"pinned: {pinned_short} | head: {head_short} | "
            f"pending: {p_count} commit(s) ==="
        )
        for commit in p_info.get("pending_commits", ()):
            lines.append(f"  {commit['short_sha']} {commit['subject']}")
        if slo := data.get("slo"):
            st = slo.get("stats") or {}
            lines.append(
                f"SLO Health: {slo['icon']} {slo['label']} -- "
                f"{slo['summary_text']} (target <= {slo['target_hours']:g}h, "
                f"breach > {slo['breach_hours']:g}h)"
            )
            if st.get("sample_rolls"):
                lines.append(
                    f"  Historical Convergence ({st['sample_rolls']} rolls): "
                    f"P50={st['p50_hours']:.1f}h, "
                    f"P90={st['p90_hours']:.1f}h, "
                    f"max={st['max_hours']:.1f}h, "
                    f"<=target={st['within_target_pct']:.0f}%, "
                    f"<=breach={st['within_breach_pct']:.0f}%, "
                    f"1st-try={st['first_try_pct']:.0f}% "
                    f"(avg {st['avg_attempts']:.1f} attempts)"
                )
                lines.append(
                    f"  SLO Calibration: {st.get('calibration_advice', '')}"
                )
        lines.append("Recent builds:")
        for build in data["recent_builds"]:
            bid = build.get("id")
            b_status = build.get("status")
            ct = build.get("createTime", "")
            et = build.get("endTime", "")
            ak = (
                f" -- {build['attempt_kind']}"
                if build.get("attempt_kind")
                else ""
            )
            lines.append(
                f"  - {constants.BUILDBUCKET_BUILD_BASE_URL}/{bid} "
                f"[{b_status}] ({ct} -> {et}){ak}"
            )
        if lf := data.get("last_failed_run"):
            lines.append(
                f"Last failed run: {lf['build_url']} ({lf['end_time']}) "
                f"[{lf['attempt_kind']}] -> {lf['brief_summary']}"
            )
        if data["triage_report"]:
            ak = (
                f" ({data['active_attempt_kind']})"
                if data.get("active_attempt_kind")
                else ""
            )
            lines.append(f"\nActive / Failed CL Triage{ak}:")
            lines.append(data["triage_report"])
        if pol := data.get("retrigger_policy"):
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
            lines.append(
                f"\nRetrigger Policy: {action_label} [{persist_label}] "
                f"signatures={pol.get('signatures', [])}"
            )
            if pol.get("cross_roller_matches"):
                lines.append(
                    "  Cross-roller matches: "
                    f"{', '.join(pol['cross_roller_matches'])}"
                )
            for stream_name, s_info in (
                pol.get("ci_health", {}).get("streams", {}).items()
            ):
                lines.append(
                    f"  CI {stream_name}: "
                    f"latest completed={s_info['latest_completed_status']} "
                    f"({s_info['latest_completed_id']} at "
                    f"{s_info['latest_completed_create_time']}) "
                    f"same_sig_failed={s_info['failed_with_same_signature']} "
                    f"green_since_roll={s_info['green_since_roll']}"
                )
            if pol.get("fix_signals"):
                lines.append(f"  Fix signals: {'; '.join(pol['fix_signals'])}")
            if pol.get("backoff_reasons"):
                lines.append(
                    f"  Hold/backoff reasons: {'; '.join(pol['backoff_reasons'])}"
                )
        lines.append("")

    print("\n".join(lines))


def main(argv: Sequence[str] | None = None) -> int:
    parser = _build_argument_parser()
    args = parser.parse_args(argv)

    utils.set_tool_overrides(
        bb_bin=args.bb_bin,
        luci_auth_bin=args.luci_auth_bin,
        luci_project=args.luci_project,
    )

    fuchsia_root = config.find_fuchsia_root(args.fuchsia_dir)
    wt_dir = (
        args.worktree_dir.expanduser().resolve()
        if args.worktree_dir is not None
        else None
    )
    state_path = args.state_file.expanduser().resolve()
    history_path = args.history_file.expanduser().resolve()

    try:
        if args.set_slo:
            updated_cfg = metrics.apply_slo_overrides(
                history_path, args.set_slo
            )
            slo_out = (
                sys.stderr if (args.json or args.table_only) else sys.stdout
            )
            print(f"Updated SLO configuration in {history_path}:", file=slo_out)
            for r_name, cfg in updated_cfg.items():
                print(
                    f"  - {r_name}: target <= {cfg['target_hours']:g}h, "
                    f"breach > {cfg['breach_hours']:g}h",
                    file=slo_out,
                )
            if not args.table_only and not args.json and not args.monitor:
                return 0

        if args.retrigger:
            rollers, _ = config.discover_rollers_and_pending(fuchsia_root)
            for roller_name in args.retrigger:
                resp = monitor.retrigger_roller(
                    fuchsia_root, rollers, roller_name, args.reason
                )
                r_info = rollers[roller_name]
                print(
                    f"Triggered {roller_name} "
                    f"({r_info['luci_project']}/{r_info['scheduler_job']}): "
                    f"{json.dumps(resp)}"
                )
            return 0

        monitor_log = (
            sys.stderr if (args.json or args.table_only) else sys.stdout
        )
        report, snapshot, skill_updates = collect_rollers_snapshot(
            fuchsia_root,
            state_path,
            history_path,
            worktree_dir=wt_dir,
            watched_cl_nums=args.watch_cl,
            run_monitor=args.monitor,
            update_skill=args.update_skill,
            disable_retrigger=args.disable_retrigger,
            log_stream=monitor_log,
        )
    except (
        FileNotFoundError,
        ValueError,
        subprocess.CalledProcessError,
        urllib.error.URLError,
    ) as exc:
        detail = str(exc)
        if isinstance(exc, subprocess.CalledProcessError) and exc.stderr:
            detail = f"{detail}: {exc.stderr.strip()}"
        print(f"error: {detail}", file=sys.stderr)
        return 1

    if args.json:
        print(
            json.dumps(
                {
                    "tree_status": snapshot["tree_status"],
                    "rollers": report,
                    "snapshot": snapshot,
                    "markdown_table": snapshot["markdown_table"],
                    "skill_updates": skill_updates,
                    "monitor_summary": snapshot.get("monitor_summary"),
                },
                indent=2,
            )
        )
        return 0

    if args.table_only:
        print(snapshot["markdown_table"])
        return 0

    _print_human_report(report, snapshot, skill_updates)
    return 0


if __name__ == "__main__":
    sys.exit(main())
