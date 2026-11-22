# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Markdown status table rendering, roll attempt descriptions, and SKILL.md sync."""

import json
import os
import pathlib
import re
import time
import types
from collections.abc import Mapping, Sequence
from typing import Any

from libc_rollers import utils

_CONFIG_VALUE_PATTERNS = types.MappingProxyType(
    {
        "name": r"[A-Za-z0-9_.-]{1,64}",
        "bucket": r"[A-Za-z0-9_.-]{1,64}",
        "manifest_project": r"[A-Za-z0-9_./-]{1,128}",
        "remote": r"https://[A-Za-z0-9_./-]+",
        "path": r"[A-Za-z0-9_./-]{1,128}",
        "schedule": r"[A-Za-z0-9_.* /:-]{1,64}",
    }
)


def validate_roller_config_value(key: str, val: str) -> str:
    """Validate discovered roller metadata before writing to SKILL.md or constants.py."""
    pat = _CONFIG_VALUE_PATTERNS.get(key, r"[A-Za-z0-9_./:-]{1,128}")
    if not re.fullmatch(pat, val):
        raise ValueError(f"Refusing to write invalid {key} value: {val!r}")
    return val


def describe_roll_attempt(
    step_info: Mapping[str, Any],
    prev_step_info: Mapping[str, Any] | None = None,
) -> str:
    """Describe whether a roll run restored a prior CL or added new commits."""
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

    prev_count = (
        prev_step_info.get("commit_count")
        if prev_step_info is not None
        else None
    )
    if (
        count is not None
        and prev_count is not None
        and count > prev_count
        and prev_step_info is not None
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


def render_skill_table(rollers: Mapping[str, Mapping[str, Any]]) -> str:
    """Render the Markdown table of target rollers for SKILL.md."""
    lines = [
        "<!-- BEGIN AUTO-GENERATED ROLLERS TABLE -->",
        (
            "| Roller Builder | LUCI Bucket | Upstream Mirror | "
            "Target Path in `fuchsia.git` | Schedule |"
        ),
        "| :--- | :--- | :--- | :--- | :--- |",
    ]
    for name, info in rollers.items():
        s_name = validate_roller_config_value("name", str(name))
        s_bucket = validate_roller_config_value("bucket", str(info["bucket"]))
        s_remote = validate_roller_config_value("remote", str(info["remote"]))
        s_path = validate_roller_config_value("path", str(info["path"]))
        s_sched = validate_roller_config_value(
            "schedule", str(info["schedule"])
        )
        lines.append(
            f"| `{s_name}` | `{s_bucket}` | "
            f"`{s_remote}` | `{s_path}` | `{s_sched}` |"
        )
    lines.append("<!-- END AUTO-GENERATED ROLLERS TABLE -->")
    return "\n".join(lines)


def render_default_rollers_block(
    rollers: Mapping[str, Mapping[str, Any]],
) -> str:
    """Render the `DEFAULT_ROLLERS` Python dict block for `constants.py`."""
    lines = [
        "# BEGIN AUTO-GENERATED DEFAULT_ROLLERS",
        "DEFAULT_ROLLERS = {",
    ]
    for name, info in rollers.items():
        s_name = validate_roller_config_value("name", str(name))
        lines.append(f"    {json.dumps(s_name)}: {{")
        for key in (
            "bucket",
            "manifest_project",
            "remote",
            "path",
            "schedule",
        ):
            s_val = validate_roller_config_value(key, str(info[key]))
            single_line = f"        {json.dumps(key)}: {json.dumps(s_val)},"
            if len(single_line) <= 80:
                lines.append(single_line)
            else:
                lines.append(f"        {json.dumps(key)}: (")
                lines.append(f"            {json.dumps(s_val)}")
                lines.append("        ),")
        lines.append("    },")
    lines.append("}")
    lines.append("# END AUTO-GENERATED DEFAULT_ROLLERS")
    return "\n".join(lines)


def replace_delimited_section(
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
    rollers: Mapping[str, Mapping[str, Any]],
    *,
    force_write: bool = False,
    worktree_dir: pathlib.Path | None = None,
) -> list[str]:
    """Check if SKILL.md or `constants.py` drifted from LUCI config and sync."""
    updates: list[str] = []
    if (
        worktree_dir is not None
        and (
            worktree_dir / "sdk" / "lib" / "c" / "skills" / "libc-rollers"
        ).is_dir()
    ):
        skill_dir = (
            worktree_dir / "sdk" / "lib" / "c" / "skills" / "libc-rollers"
        )
        constants_path = skill_dir / "scripts" / "libc_rollers" / "constants.py"
        skill_md_path = skill_dir / "SKILL.md"
    else:
        pkg_dir = pathlib.Path(__file__).resolve().parent
        constants_path = pkg_dir / "constants.py"
        skill_md_path = pkg_dir.parents[1] / "SKILL.md"

    if skill_md_path.is_file():
        old_text = skill_md_path.read_text(encoding="utf-8")
        new_table = render_skill_table(rollers)
        new_text = replace_delimited_section(
            old_text,
            "<!-- BEGIN AUTO-GENERATED ROLLERS TABLE -->",
            "<!-- END AUTO-GENERATED ROLLERS TABLE -->",
            new_table,
        )
        if new_text != old_text:
            if force_write and os.access(skill_md_path, os.W_OK):
                skill_md_path.write_text(new_text, encoding="utf-8")
                updates.append(f"Self-updated LUCI table in {skill_md_path}")
            else:
                updates.append(
                    f"Detected LUCI config drift in {skill_md_path} "
                    "(run with --update-skill to write)"
                )

    if constants_path.is_file():
        old_py = constants_path.read_text(encoding="utf-8")
        new_block = render_default_rollers_block(rollers)
        new_py = replace_delimited_section(
            old_py,
            "# BEGIN AUTO-GENERATED DEFAULT_ROLLERS",
            "# END AUTO-GENERATED DEFAULT_ROLLERS",
            new_block,
        )
        if new_py != old_py:
            if force_write and os.access(constants_path, os.W_OK):
                constants_path.write_text(new_py, encoding="utf-8")
                updates.append(
                    f"Self-updated DEFAULT_ROLLERS in {constants_path}"
                )
            else:
                updates.append(
                    f"Detected LUCI config drift in {constants_path} "
                    "(run with --update-skill to write)"
                )
    return updates


def render_markdown_status_table(
    roller_rows: Sequence[Mapping[str, Any]],
    watched_cls: Sequence[Mapping[str, Any] | None] | None = None,
    now_sec: float | None = None,
) -> str:
    """Render the concise 4-column Markdown status table with SLO indicators."""
    now = now_sec if now_sec is not None else time.time()
    lines = [
        (
            "| Roller / CL & SLO Health | "
            "Pinned → Head (Pending) & Last Landed | "
            "Pending Attempt | Last Failed Attempt |"
        ),
        "| :--- | :--- | :--- | :--- |",
    ]
    for row in roller_rows:
        name = row["name"]
        slo = row.get("slo") or {}
        slo_icon = slo.get("icon", "🟢")
        slo_label = slo.get("label", "Within SLO")
        slo_summary = slo.get("summary_text", "")
        slo_stats = slo.get("stats") or {}
        stats_line = ""
        if slo_stats.get("sample_rolls"):
            stats_line = (
                f"<br>*(P50 {slo_stats['p50_hours']:.1f}h · "
                f"P90 {slo_stats['p90_hours']:.1f}h · "
                f"{slo_stats['within_target_pct']:.0f}% "
                f"≤{slo.get('target_hours', 12):g}h)*"
            )
        col1_roller = (
            f"{slo_icon} **`{name}`** -- **{slo_label}**<br>"
            f"{slo_summary}{stats_line}"
        )

        pinned = row["pinned_revision"]
        head = row["head_revision"]
        p_count = row["pending_count"]
        if p_count > 0:
            rev_str = f"`{pinned}` → `{head}` (**{p_count} pending**)"
            if slo.get("oldest_pending_epoch") is not None:
                oldest_str = utils.format_time_pair(
                    slo["oldest_pending_epoch"], now
                )
                rev_str += f"<br>Oldest pending: {oldest_str}"
        else:
            rev_str = f"`{pinned}` (**0 pending**, up to date)"

        landed = row.get("last_landed") or {}
        if landed.get("epoch") is not None:
            t_str = utils.format_time_pair(landed["epoch"], now)
            cl_part = (
                f" via [fxr/{landed['cl_num']}]({landed['cl_url']})"
                if landed.get("cl_url")
                else ""
            )
            sha_part = (
                f" (`{landed['commit']}`)" if landed.get("commit") else ""
            )
            rev_str += f"<br>Landed {t_str}{cl_part}{sha_part}"

        pa = row.get("pending_attempt")
        if pa:
            cl_md = (
                f"[fxr/{pa['cl_num']}/{pa.get('patchset') or '1'}]"
                f"({pa['cl_url']})"
                if pa.get("cl_url")
                else "*creating CL*"
            )
            b_md = (
                f"[`{pa['build_id']}`]({pa['build_url']})"
                if pa.get("build_url")
                else "build"
            )
            t_md = (
                f" {utils.format_time_pair(pa['start_epoch'], now)}"
                if pa.get("start_epoch") is not None
                else ""
            )
            cq = pa.get("cq")
            cq_md = (
                f", `{cq['passed']}/{cq['total']}` passed, "
                f"`{cq['failed']}` failed, `{cq['running']}` running"
                if cq and cq.get("total")
                else ""
            )
            col3 = (
                f"{cl_md} ({b_md}): `{pa['status']}`{t_md}, "
                f"{pa.get('attempt_kind', '')}{cq_md}"
            )
        else:
            col3 = "--"

        lf = row.get("last_failed_attempt")
        if p_count > 0 and lf:
            cl_md = (
                f"[fxr/{lf['cl_num']}/{lf.get('patchset') or '1'}]"
                f"({lf['cl_url']})"
                if lf.get("cl_url") and lf.get("cl_num")
                else "no CL"
            )
            b_md = (
                f"[`{lf['build_id']}`]({lf['build_url']})"
                if lf.get("build_url")
                else "build"
            )
            t_md = (
                f" {utils.format_time_pair(lf['end_epoch'], now)}"
                if lf.get("end_epoch") is not None
                else ""
            )
            summary_str = (lf.get("failure_summary") or "").replace("|", "\\|")
            col4 = (
                f"{cl_md} ({b_md}): ended{t_md}, "
                f"{lf.get('attempt_kind', '')} -- {summary_str}"
            )
        else:
            col4 = "-- *(up to date)*"

        lines.append(f"| {col1_roller} | {rev_str} | {col3} | {col4} |")

    for sc in watched_cls or ():
        if not sc:
            continue
        ps_suffix = f"/{sc['patchset']}" if sc.get("patchset") else ""
        col1 = (
            f"**[fxr/{sc['cl_num']}{ps_suffix}]({sc['cl_url']})**<br>"
            f"({sc.get('subject', '')})"
        )
        up_t = (
            f" (PS{sc['patchset']} uploaded "
            f"{utils.format_time_pair(sc['ps_created_epoch'], now)})"
            if sc.get("ps_created_epoch") is not None
            else ""
        )
        col2 = f"`{sc.get('status', 'UNKNOWN')}`{up_t}"
        cq = sc.get("cq")
        col3 = (
            f"`{cq['passed']}/{cq['total']}` CQ tryjobs passed "
            f"(`{cq['failed']}` failed, `{cq['running']}` running)"
            if cq and cq.get("total")
            else "No active CQ run"
        )
        col4 = f"`{sc.get('unresolved_comment_count', 0)}` unresolved comments"
        lines.append(f"| {col1} | {col2} | {col3} | {col4} |")

    return "\n".join(lines)
