# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Persistent roller convergence telemetry, SLO evaluation, and threshold calibration."""

import datetime
import json
import pathlib
import re
from collections.abc import Mapping, MutableMapping, Sequence
from typing import Any

from libc_rollers import constants, gerrit, utils


def default_roller_slo_cfg(
    roller_name: str, schedule: str = ""
) -> dict[str, Any]:
    """Return a mutable copy of default SLO thresholds for a roller."""
    if roller_name in constants.DEFAULT_SLO_CONFIG:
        return dict(constants.DEFAULT_SLO_CONFIG[roller_name])
    is_frequent = "interval" in schedule
    return {
        "target_hours": 12.0 if is_frequent else 18.0,
        "breach_hours": 24.0 if is_frequent else 36.0,
        "target_compliance_pct": 80.0,
        "cadence_note": f"schedule: {schedule or 'triggered'}",
    }


def load_health_history(history_file: pathlib.Path) -> dict[str, Any]:
    """Load persistent roller health telemetry and SLO configuration from disk."""
    history: dict[str, Any] = {
        "schema_version": 1,
        "updated_at": None,
        "slo_config": {
            k: dict(v) for k, v in constants.DEFAULT_SLO_CONFIG.items()
        },
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
                                r_name, default_roller_slo_cfg(r_name)
                            ).update(r_cfg)
                if isinstance(loaded.get("rollers"), dict):
                    history["rollers"] = loaded["rollers"]
        except (OSError, json.JSONDecodeError, ValueError):
            pass
    return history


def save_health_history(
    history_file: pathlib.Path, history: MutableMapping[str, Any]
) -> None:
    """Atomically persist roller health telemetry and SLO configuration."""
    history["updated_at"] = datetime.datetime.now(
        datetime.timezone.utc
    ).isoformat()
    utils.atomic_write_json(history_file, history)


def apply_slo_overrides(
    history_file: pathlib.Path, overrides: Sequence[str]
) -> dict[str, Any]:
    """Update per-roller SLO target/breach hours in `--history-file`."""
    history = load_health_history(history_file)
    for spec in overrides:
        match = re.fullmatch(
            r"([A-Za-z0-9_.-]+)=([0-9]+(?:\.[0-9]+)?):([0-9]+(?:\.[0-9]+)?)",
            spec.strip(),
        )
        if match is None:
            raise ValueError(
                f"Invalid --set-slo spec {spec!r}; expected "
                "<roller>=<target_hours>:<breach_hours> "
                "(e.g. llvm-libc-roller=12:24)"
            )
        r_name, target_s, breach_s = (
            match.group(1),
            match.group(2),
            match.group(3),
        )
        target_h = float(target_s)
        breach_h = float(breach_s)
        if target_h <= 0 or breach_h <= target_h:
            raise ValueError(
                f"Invalid SLO thresholds for {r_name}: target_hours "
                f"({target_h}) must be > 0 and < breach_hours ({breach_h})"
            )
        cfg = history["slo_config"].setdefault(
            r_name, default_roller_slo_cfg(r_name)
        )
        cfg["target_hours"] = target_h
        cfg["breach_hours"] = breach_h
        cfg["updated_at"] = datetime.datetime.now(
            datetime.timezone.utc
        ).isoformat()
    save_health_history(history_file, history)
    slo_cfg: dict[str, Any] = history["slo_config"]
    return slo_cfg


def update_and_evaluate_roller_slo(
    *,
    history: MutableMapping[str, Any],
    roller_name: str,
    manifest_project: str,
    schedule: str,
    p_info: Mapping[str, Any],
    last_landed: Mapping[str, Any],
    recent_builds: Sequence[Mapping[str, Any]],
    failure_classification: Mapping[str, Any] | None,
    pending_attempt: Mapping[str, Any] | None,
    now_epoch: float,
) -> dict[str, Any]:
    """Update historical roll/build stats for a roller and evaluate its SLO tier."""
    slo_cfg = history.setdefault("slo_config", {}).setdefault(
        roller_name, default_roller_slo_cfg(roller_name, schedule)
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

    existing_rolls: dict[str, dict[str, Any]] = {
        str(r["cl_num"]): dict(r)
        for r in r_hist.get("merged_rolls", ())
        if isinstance(r, dict) and r.get("cl_num")
    }
    last_landed_cl = str(last_landed.get("cl_num") or "")
    last_backfill = float(r_hist.get("last_gerrit_backfill_epoch") or 0.0)
    if (
        not existing_rolls
        or (last_landed_cl and last_landed_cl not in existing_rolls)
        or (now_epoch - last_backfill >= 1800.0)
    ):
        fetched_rolls = gerrit.fetch_recent_merged_rolls(
            manifest_project, limit=15
        )
        if fetched_rolls:
            for fr in fetched_rolls:
                existing_rolls[str(fr["cl_num"])] = fr
            r_hist["last_gerrit_backfill_epoch"] = now_epoch

    merged_list = sorted(
        existing_rolls.values(),
        key=lambda item: float(item.get("submitted_epoch") or 0.0),
        reverse=True,
    )[:50]
    r_hist["merged_rolls"] = merged_list

    existing_builds: dict[str, dict[str, Any]] = {
        str(b["build_id"]): dict(b)
        for b in r_hist.get("builds", ())
        if isinstance(b, dict) and b.get("build_id")
    }
    for build in recent_builds:
        bid = str(build.get("id") or "")
        st = build.get("status", "")
        if not bid or st not in ("SUCCESS", "FAILURE", "INFRA_FAILURE"):
            continue
        c_ep = utils.iso_to_epoch(build.get("createTime"))
        e_ep = utils.iso_to_epoch(
            build.get("endTime") or build.get("createTime")
        )
        if st in ("FAILURE", "INFRA_FAILURE"):
            f_kind = "flake" if build.get("flake_signatures") else "failure"
        else:
            f_kind = "none"
        existing_builds[bid] = {
            "build_id": bid,
            "status": st,
            "create_epoch": c_ep,
            "end_epoch": e_ep,
            "attempt_kind": build.get("attempt_kind", ""),
            "failure_kind": f_kind,
        }
    builds_list = sorted(
        existing_builds.values(),
        key=lambda item: float(item.get("create_epoch") or 0.0),
        reverse=True,
    )[:100]
    r_hist["builds"] = builds_list

    conv_vals = sorted(
        float(r["convergence_hours"])
        for r in merged_list
        if r.get("convergence_hours") is not None
    )
    sample_rolls = len(conv_vals)
    p50_h = utils.percentile(conv_vals, 50.0)
    p90_h = utils.percentile(conv_vals, 90.0)
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

    if sample_rolls < 5:
        calibration_advice = "Collecting baseline roll history (<5 rolls)."
    else:
        suggested_target = max(4.0, round(p90_h * 2.0) / 2.0)
        suggested_breach = max(breach_h, round(suggested_target * 2))
        if within_target_pct < target_comp_pct and avg_attempts >= 1.8:
            calibration_advice = (
                f"{within_target_pct:.0f}% <= {target_h:g}h target "
                f"(goal >={target_comp_pct:.0f}%) due to multi-attempt CQ flake "
                f"retries (avg {avg_attempts:.1f} tries, P90 {p90_h:.1f}h). "
                f"Keep {target_h:g}h/{breach_h:g}h to highlight flake lag, or "
                f"widen with --set-slo "
                f"{roller_name}={suggested_target:g}:{suggested_breach:g}."
            )
        elif within_target_pct < target_comp_pct:
            calibration_advice = (
                f"{within_target_pct:.0f}% <= {target_h:g}h target "
                f"(P90 {p90_h:.1f}h); consider loosening via --set-slo "
                f"{roller_name}={suggested_target:g}:{suggested_breach:g}."
            )
        elif within_target_pct >= 95.0 and p90_h < target_h * 0.65:
            tight_t = max(4.0, round((p90_h * 1.25) * 2.0) / 2.0)
            calibration_advice = (
                f"High compliance ({within_target_pct:.0f}% <= {target_h:g}h, "
                f"P90 {p90_h:.1f}h); can tighten via --set-slo "
                f"{roller_name}={tight_t:g}:{round(tight_t * 2):g}."
            )
        else:
            calibration_advice = (
                f"Well-calibrated ({within_target_pct:.0f}% <= {target_h:g}h "
                f"target, {within_breach_pct:.0f}% <= {breach_h:g}h breach; "
                f"P50 {p50_h:.1f}h, P90 {p90_h:.1f}h)."
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

    pending_count = int(p_info.get("pending_count", 0))
    oldest_ep = p_info.get("oldest_pending_epoch")
    lag_hours = (
        round(max(0.0, (now_epoch - float(oldest_ep)) / 3600.0), 1)
        if (pending_count > 0 and oldest_ep is not None)
        else 0.0
    )

    consecutive_failures = 0
    if pending_count > 0:
        for build in recent_builds:
            st = build.get("status", "")
            if st in ("STARTED", "SCHEDULED"):
                continue
            if st in ("FAILURE", "INFRA_FAILURE"):
                consecutive_failures += 1
            elif st == "SUCCESS":
                break

    actionable_list = (
        (failure_classification or {}).get("actionable", ())
        if pending_count > 0
        else ()
    )
    actionable_count = len(actionable_list)
    active_cq_failed = (
        int((pending_attempt.get("cq") or {}).get("failed", 0))
        if (pending_count > 0 and pending_attempt is not None)
        else 0
    )

    if pending_count > 0 and (
        actionable_count > 0
        or lag_hours > breach_h
        or consecutive_failures >= 3
    ):
        status = "breach"
        label = "SLO Breached"
        icon = "🔴"
        reasons: list[str] = []
        if actionable_count > 0:
            reasons.append(f"{actionable_count} actionable failure(s)")
        if lag_hours > breach_h:
            reasons.append(f"lag ~{lag_hours:.1f}h > {breach_h:g}h breach")
        if consecutive_failures >= 3:
            reasons.append(
                f"{consecutive_failures} consecutive failed attempts"
            )
        reason = "; ".join(reasons)
        summary_text = f"Lag ~{lag_hours:.1f}h / {breach_h:g}h max ({reason})"
    elif pending_count > 0 and (
        lag_hours > target_h
        or consecutive_failures == 2
        or active_cq_failed > 0
    ):
        status = "warning"
        label = "SLO At Risk"
        icon = "🟡"
        reasons = []
        if lag_hours > target_h:
            reasons.append(
                f"lag ~{lag_hours:.1f}h > {target_h:g}h target "
                f"(breach >{breach_h:g}h)"
            )
        if consecutive_failures == 2:
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
        "consecutive_failures": consecutive_failures,
        "actionable_failures": actionable_count,
        "active_cq_failures": active_cq_failed,
        "reason": reason,
        "summary_text": summary_text,
        "stats": stats,
    }
