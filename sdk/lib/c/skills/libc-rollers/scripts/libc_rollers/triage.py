# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Buildbucket summary parsing, failure signature extraction, and flake classification."""

import datetime
import pathlib
import re
import subprocess
from collections.abc import Collection, Mapping, MutableMapping, Sequence
from typing import Any

from libc_rollers import constants, gerrit, luci, utils

_STEP_TOKENS = (
    "bazel_tests_utils.py",
    "verify_bazel2gn",
    "bazel_target_can_only_be_referenced_by_one_bazel_action",
    "mypy_checked",
    "Fastboot_error",
    "Timeout_attempting_to_reach_target",
    "botanist_ERROR",
    "sys_config",
    "compare built artifacts in gen/build/bazel",
)
_SUBBUILD_SUFFIXES = ("-subbuild-swarming", "-subbuild")


def _is_superseded_parent_wrapper(
    builder_name: str, failed_names: Collection[str]
) -> bool:
    """Return True if `builder_name` is a wrapper whose subbuild also failed."""
    if builder_name == "run-postsubmit-tryjobs":
        return True
    return any(
        f"{builder_name}{suffix}" in failed_names
        for suffix in _SUBBUILD_SUFFIXES
    )


def _strip_subbuild_suffix(builder_name: str) -> str:
    """Strip `-subbuild-swarming` or `-subbuild` suffix from a builder name."""
    for suffix in _SUBBUILD_SUFFIXES:
        if builder_name.endswith(suffix):
            return builder_name.removesuffix(suffix)
    return builder_name


def first_meaningful_summary_line(summary_md: str) -> str:
    """Return a concise single-line summary of a Buildbucket summaryMarkdown."""
    lines = [
        stripped
        for line in (summary_md or "").splitlines()
        if (stripped := line.strip()) and not stripped.startswith("```")
    ]
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
        bullets: list[str] = []
        for extra in lines[1:]:
            if extra.startswith(("-", "*")):
                item = extra.lstrip("-* ").strip()
                if m_pkg := re.search(
                    r"fuchsia-pkg://[^/\s`]+/([^#\s`]+)(?:#meta/([^\s`]+))?",
                    item,
                ):
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


def format_downstream_leaf_note(
    leaf_builds: Sequence[Mapping[str, Any]],
) -> str:
    """Format a short bracketed note summarizing leaf child build failures."""
    if not leaf_builds:
        return ""
    parts: list[str] = []
    for lb in leaf_builds[:2]:
        lb_status = lb.get("status", "FAILURE")
        lb_first = first_meaningful_summary_line(
            lb.get("summaryMarkdown", "") or ""
        )
        parts.append(
            f"leaf {lb_status}: {lb_first}" if lb_first else f"leaf {lb_status}"
        )
    if len(leaf_builds) > 2:
        parts.append(f"+{len(leaf_builds) - 2} more")
    return f" [{'; '.join(parts)}]"


def extract_ninja_failed_targets(summary_md: str) -> list[str]:
    """Extract normalized Ninja output target names from FAILED lines."""
    targets: list[str] = []
    for match in re.finditer(
        r"^[ \t]*FAILED:[ \t]*(?:\[code=\d+\][ \t]+)?([^\r\n]+)",
        summary_md or "",
        re.MULTILINE,
    ):
        line_rest = match.group(1).strip()
        if not line_rest or re.match(
            r"^(?:Build\s+did\s+not\b|Streaming\s+build\b)",
            line_rest,
            re.IGNORECASE,
        ):
            continue
        line_targets: list[str] = []
        fallback_targets: list[str] = []
        for raw_tok in line_rest.split():
            tok = raw_tok.strip("`'\",;")
            if (
                not tok
                or tok.startswith(("-", "../../", "/"))
                or "=" in tok
                or tok in ("&&", "||", ";", "|")
            ):
                break
            if not re.fullmatch(r"[A-Za-z0-9_./+-]+", tok):
                continue
            if tok.endswith(".clippy") and tok.startswith("gen/"):
                norm = tok.removeprefix("gen/")
            else:
                norm = tok.rsplit("/", 1)[-1]
            if not norm:
                continue
            if norm.endswith(constants.NINJA_AUX_SUFFIXES):
                fallback_targets.append(norm)
            else:
                line_targets.append(norm)
        for cand in line_targets or fallback_targets[:1]:
            if cand not in targets:
                targets.append(cand)
    return targets


def extract_failure_signatures(builder: str, summary_md: str) -> list[str]:
    """Extract normalized failure signatures for tracking persistent failures."""
    text = summary_md or ""
    sigs: list[str] = []
    for match in re.finditer(
        r"fuchsia-pkg://[^/\s`]+/([^#\s`]+)(?:#meta/([^\s`]+))?",
        text,
    ):
        pkg, comp = match.group(1), match.group(2)
        if comp and comp not in (
            "integration_test.cm",
            "test-root.cm",
            "default.cm",
        ):
            sigs.append(f"test:{comp}")
        else:
            sigs.append(f"test:{pkg}")

    for ninja_target in extract_ninja_failed_targets(text):
        sigs.append(f"target:{ninja_target}")

    for m_host in re.finditer(
        r"(?:host_x64|linux_x64|host_arm64)/(?!obj/)[^\s`]+/([A-Za-z0-9_.-]+)",
        text,
    ):
        host_bin = m_host.group(1)
        if not host_bin.endswith(
            (*constants.NINJA_AUX_SUFFIXES, ".json", ".o", ".a", ".so")
        ):
            sigs.append(f"test:{host_bin}")

    for m_task in re.finditer(
        r"failed to process test results for \d+ tasks?:\s*([^\s`\[]+)",
        text,
        re.IGNORECASE,
    ):
        task_name = re.sub(r"-\(\d+\)$", "", m_task.group(1).strip())
        sigs.append(f"task:{task_name}")

    lower_text = text.lower()
    for tok in _STEP_TOKENS:
        if tok.lower() in lower_text:
            sigs.append(f"step:{tok}")

    if not sigs:
        norm_builder = re.sub(r"-rust(?=-|$)", "", builder)
        sigs.append(f"builder:{norm_builder}")

    return list(dict.fromkeys(sigs))


def extract_failure_keywords(signatures: Collection[str]) -> list[str]:
    """Extract search keywords from failure signatures to match gardener reverts."""
    keywords: set[str] = set()
    for sig in signatures:
        kind, _, val = sig.partition(":")
        if kind == "builder" or not val:
            continue
        if val in (
            "verify_bazel2gn",
            "bazel_target_can_only_be_referenced_by_one_bazel_action",
        ) or val.endswith(
            ".bazel_target_can_only_be_referenced_by_one_bazel_action"
        ):
            keywords.update(("bazel2gn", "bazel_migration"))
        clean = val
        if "/" in clean:
            clean = re.sub(r"^(?:src|zircon|sdk|tools|vendor)/", "", clean)
            clean = re.sub(
                r"/(?:bin|lib)(?:_(?:test|unittests))?(?:\.clippy)?$",
                "",
                clean,
            )
        clean = re.sub(
            r"\.(?:cm|py|sh|cc|rs|clippy|"
            r"bazel_target_can_only_be_referenced_by_one_bazel_action)(?:\.host)?$",
            "",
            clean,
        )
        clean = re.sub(r"\.host$", "", clean)
        clean = re.sub(
            r"[-_](?:(?:bin|lib|integration|unit)[-_])?tests?$",
            "",
            clean,
            flags=re.IGNORECASE,
        )
        low = clean.lower()
        if len(low) >= 4 and low not in constants.GENERIC_DIRS:
            keywords.add(low)
            norm_sep = low.replace("/", "_").replace("-", "_")
            if norm_sep not in constants.GENERIC_DIRS:
                keywords.add(norm_sep)
                keywords.add(norm_sep.replace("_", "-"))
            # Gardener reverts name the component (e.g. `[pkg-resolver]`),
            # not its source directory, so key on the last path component.
            base = low.rsplit("/", 1)[-1]
            if (
                base != low
                and len(base) >= 4
                and base not in constants.GENERIC_DIRS
            ):
                base_sep = base.replace("-", "_")
                keywords.update((base, base_sep, base_sep.replace("_", "-")))
            else:
                base_sep = norm_sep
            parts = [p for p in base_sep.split("_") if p]
            if len(parts) >= 2:
                keywords.add("".join(f"[{p}]" for p in parts[:3]))
    return sorted(keywords)


def is_unrelated_leaf_build(leaf_build: Mapping[str, Any]) -> bool:
    """Return True when a resolved leaf child build is an unrelated flake."""
    lb_status = leaf_build.get("status", "FAILURE")
    lb_summary = leaf_build.get("summaryMarkdown", "") or ""
    if constants.REAL_FAILURE_PATTERNS.search(lb_summary):
        return False
    lb_first = first_meaningful_summary_line(lb_summary)
    return (
        lb_status == "INFRA_FAILURE"
        or bool(constants.UNRELATED_FLAKE_PATTERNS.search(lb_summary))
        or bool(re.search(r"^\d+\s+tests?\s+failed:", lb_first, re.IGNORECASE))
    )


def run_triage_cl(
    fuchsia_root: pathlib.Path,
    change_num: str,
    patchset: str | None = None,
    builds: Sequence[Mapping[str, Any]] | None = None,
    subbuild_cache: MutableMapping[str, list[dict[str, Any]]] | None = None,
) -> tuple[str, str]:
    """Summarize tryjobs on a Gerrit CL and return `(report_md, brief_summary)`."""
    if builds is None:
        try:
            cl_url, fetched_builds = luci.fetch_cl_builds(
                fuchsia_root, change_num, patchset
            )
            resolved_builds: Sequence[Mapping[str, Any]] = fetched_builds
        except (OSError, subprocess.SubprocessError) as exc:
            cl_url = (
                gerrit.format_cl_url(change_num, patchset or "1")
                or f"{constants.GERRIT_BASE_URL}/c/fuchsia/+/{change_num}/1"
            )
            err_msg = str(exc)
            return err_msg, f"CL {cl_url}: {err_msg}"
    else:
        cl_url = (
            gerrit.format_cl_url(change_num, patchset or "1")
            or f"{constants.GERRIT_BASE_URL}/c/fuchsia/+/{change_num}/1"
        )
        resolved_builds = builds

    lines = [f"# CI Triage Report for CL: {cl_url}", ""]
    if not resolved_builds:
        lines.append("No builds found for this CL on Buildbucket.")
        return "\n".join(lines), f"CL {cl_url}: no tryjob builds found"

    success = [b for b in resolved_builds if b.get("status") == "SUCCESS"]
    failed = [
        b
        for b in resolved_builds
        if b.get("status") in ("FAILURE", "INFRA_FAILURE")
    ]
    running = [
        b
        for b in resolved_builds
        if b.get("status") in ("STARTED", "SCHEDULED")
    ]

    lines.append(
        f"**Summary:** Total Builds: `{len(resolved_builds)}` | "
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
            f"{cl_url} ({len(success)}/{len(resolved_builds)} passed, "
            f"0 failed, {len(running)} running)"
        )
        return "\n".join(lines), brief

    lines.append(f"## Failing Tryjobs ({len(failed)})")
    lines.append("")
    lines.append("| Builder | Status | Build ID | Summary |")
    lines.append("| :--- | :--- | :--- | :--- |")
    failed_names = {b.get("builder", {}).get("builder", "") for b in failed}
    brief_items: list[str] = []
    for build in sorted(
        failed, key=lambda b: str(b.get("builder", {}).get("builder", ""))
    ):
        bid = build.get("id", "unknown")
        b_name = str(build.get("builder", {}).get("builder", "unknown"))
        st = build.get("status", "FAILURE")
        raw_summary = build.get("summaryMarkdown", "") or ""
        first_line = first_meaningful_summary_line(raw_summary)
        if constants.DOWNSTREAM_WRAPPER_PATTERN.search(raw_summary):
            leaf_builds = luci.resolve_downstream_leaf_builds(
                fuchsia_root, str(bid), cache=subbuild_cache
            )
            first_line += format_downstream_leaf_note(leaf_builds)
        lines.append(f"| **{b_name}** | `{st}` | `{bid}` | {first_line} |")
        if _is_superseded_parent_wrapper(b_name, failed_names):
            continue
        display_name = _strip_subbuild_suffix(b_name)
        brief_items.append(
            f"{display_name} ({first_line})" if first_line else display_name
        )
    if not brief_items:
        brief_items = [
            str(b.get("builder", {}).get("builder", "unknown")) for b in failed
        ]
    shown = "; ".join(brief_items[:3])
    if len(brief_items) > 3:
        shown += f"; +{len(brief_items) - 3} more"
    brief = f"{cl_url} ({len(failed)}/{len(resolved_builds)} failed: {shown})"
    return "\n".join(lines), brief


def summarize_cq_builds(
    fuchsia_root: pathlib.Path,
    cl_num: str,
    patchset: str | None,
    builds: Sequence[Mapping[str, Any]],
) -> dict[str, Any]:
    """Summarize passing, failing, and running tryjobs for a roll CL."""
    counts = gerrit.count_cq_build_statuses(builds)
    failure_summary: str | None = None
    if counts["failed"] > 0:
        _, failure_summary = run_triage_cl(
            fuchsia_root, cl_num, patchset, builds=builds
        )
    return {
        **counts,
        "failure_summary": failure_summary,
    }


def check_ci_builders_for_flakes(
    fuchsia_root: pathlib.Path,
    luci_project: str,
    flakes: Sequence[Mapping[str, Any]],
    flake_signatures: Collection[str],
    since_dt: datetime.datetime | None,
) -> dict[str, Any]:
    """Check corresponding `global.ci` builders for matching failures or recovery."""
    unique_builders = sorted(
        {str(f["builder"]) for f in flakes if f.get("builder")}
    )[:4]
    sig_set = set(flake_signatures)
    now = datetime.datetime.now(datetime.timezone.utc)

    ci_streams: dict[str, dict[str, Any]] = {}
    matched_signatures: set[str] = set()
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
                builds = luci.fetch_recent_builds(
                    fuchsia_root, ci_builder, count=5
                )
            except (OSError, subprocess.SubprocessError):
                continue
            if not builds:
                continue
            latest_dt = utils.parse_iso_utc(builds[0].get("createTime"))
            if (
                latest_dt is not None
                and (now - latest_dt).total_seconds() > 7 * 86400
            ):
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
            for build in completed:
                if build.get("status") in ("FAILURE", "INFRA_FAILURE"):
                    b_sigs = set(
                        extract_failure_signatures(
                            b_name, build.get("summaryMarkdown", "")
                        )
                    )
                    overlap = b_sigs & sig_set
                    if overlap:
                        matched_signatures.update(overlap)
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
            latest_comp_dt = utils.parse_iso_utc(latest_comp.get("createTime"))
            latest_comp_sigs = set(
                extract_failure_signatures(
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
                or (latest_comp_dt is not None and latest_comp_dt > since_dt)
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
                "latest_completed_summary": first_meaningful_summary_line(
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
        "matched_signatures": sorted(matched_signatures),
        "ci_also_failed": ci_also_failed,
        "ci_still_failing": ci_still_failing,
        "ci_persistently_failing": ci_persistently_failing,
        "ci_green_after_roll": checked_any and all_streams_green_since_roll,
        "ci_two_green_after_roll": (
            checked_any and all_streams_two_green_since_roll
        ),
    }


def classify_roll_failures(
    roller_name: str,
    latest_build: Mapping[str, Any],
    latest_steps: Mapping[str, Any],
    cl_builds: Sequence[Mapping[str, Any]],
    fuchsia_root: pathlib.Path | None = None,
    subbuild_cache: MutableMapping[str, list[dict[str, Any]]] | None = None,
    luci_project: str = "fuchsia",
) -> dict[str, Any]:
    """Classify a roller build's failures into unrelated flakes vs actionable issues."""
    failed = [
        b for b in cl_builds if b.get("status") in ("FAILURE", "INFRA_FAILURE")
    ]
    failed_names = {b.get("builder", {}).get("builder", "") for b in failed}
    leaf_failed = [
        b
        for b in sorted(
            failed,
            key=lambda item: str(item.get("builder", {}).get("builder", "")),
        )
        if not _is_superseded_parent_wrapper(
            str(b.get("builder", {}).get("builder", "")), failed_names
        )
    ]

    actionable: list[dict[str, Any]] = []
    flakes: list[dict[str, Any]] = []
    flake_signatures: list[str] = []
    exonerated_targets: set[str] = set()

    def _record_flake(flake_entry: dict[str, Any], note: str = "") -> None:
        if note and note not in flake_entry["first_line"]:
            flake_entry[
                "first_line"
            ] = f"{flake_entry['first_line']} {note}".strip()
        flakes.append(flake_entry)
        for sig in flake_entry.get("signatures", ()):
            if sig not in flake_signatures:
                flake_signatures.append(sig)

    for build in leaf_failed:
        b_name = _strip_subbuild_suffix(
            str(build.get("builder", {}).get("builder", "unknown"))
        )
        summary = build.get("summaryMarkdown", "") or ""
        first_line = first_meaningful_summary_line(summary)
        sigs = extract_failure_signatures(b_name, summary)
        ninja_targets = extract_ninja_failed_targets(summary)
        entry: dict[str, Any] = {
            "build_id": build.get("id", "unknown"),
            "builder": b_name,
            "status": build.get("status", "FAILURE"),
            "first_line": first_line,
            "summary": summary,
            "signatures": sigs,
            "ninja_targets": ninja_targets,
            "matched_real_pattern": False,
        }
        if constants.REAL_FAILURE_PATTERNS.search(summary):
            entry["matched_real_pattern"] = True
            actionable.append(entry)
        elif (
            build.get("status") == "INFRA_FAILURE"
            or constants.UNRELATED_FLAKE_PATTERNS.search(summary)
            or re.search(r"^\d+\s+tests?\s+failed:", first_line, re.IGNORECASE)
        ):
            if "false rejection" in summary.lower():
                exonerated_targets.update(ninja_targets)
            _record_flake(entry)
        elif (
            fuchsia_root is not None
            and constants.DOWNSTREAM_WRAPPER_PATTERN.search(summary)
        ):
            leaf_builds = luci.resolve_downstream_leaf_builds(
                fuchsia_root, str(build.get("id", "")), cache=subbuild_cache
            )
            entry["first_line"] += format_downstream_leaf_note(leaf_builds)
            if leaf_builds and all(
                is_unrelated_leaf_build(lb) for lb in leaf_builds
            ):
                _record_flake(entry)
            else:
                actionable.append(entry)
        else:
            actionable.append(entry)

    # Propagate LUCI [false rejection] exonerations across sibling builders on
    # the same CL that failed on the exact same non-libc Ninja target(s).
    if exonerated_targets and actionable:
        remaining_actionable: list[dict[str, Any]] = []
        for entry in actionable:
            entry_targets = set(entry.get("ninja_targets") or ())
            if (
                not entry.get("matched_real_pattern")
                and entry_targets
                and entry_targets <= exonerated_targets
            ):
                _record_flake(entry, "[sibling false rejection]")
            else:
                remaining_actionable.append(entry)
        actionable = remaining_actionable

    # When no builder matched REAL_FAILURE_PATTERNS, check whether remaining
    # fallback failures are also failing on global.ci or were reverted on
    # fuchsia.git main after the roll started.
    has_confirmed_real_failure = any(
        e.get("matched_real_pattern") for e in actionable
    )
    fallback_entries = [
        e for e in actionable if not e.get("matched_real_pattern")
    ]
    if (
        fuchsia_root is not None
        and fallback_entries
        and not has_confirmed_real_failure
    ):
        specific_sigs = sorted(
            {
                s
                for e in fallback_entries
                for s in e.get("signatures", ())
                if not s.startswith("builder:")
            }
        )
        if specific_sigs:
            since_dt = utils.parse_iso_utc(latest_build.get("createTime"))
            ci_check = check_ci_builders_for_flakes(
                fuchsia_root,
                luci_project,
                fallback_entries,
                specific_sigs,
                since_dt,
            )
            matched_ci_sigs = set(ci_check.get("matched_signatures", ()))
            gardener_check = gerrit.fetch_gardener_and_main_fixes(
                since_dt, extract_failure_keywords(specific_sigs)
            )
            revert_subjects = [
                r.get("subject", "").lower()
                for r in gardener_check.get("reverts_since_roll", ())
            ]
            post_ci_actionable: list[dict[str, Any]] = []
            for entry in actionable:
                if entry.get("matched_real_pattern"):
                    post_ci_actionable.append(entry)
                    continue
                entry_sigs = [
                    s
                    for s in entry.get("signatures", ())
                    if not s.startswith("builder:")
                ]
                entry_kws = extract_failure_keywords(entry_sigs)
                if set(entry_sigs) & matched_ci_sigs:
                    _record_flake(entry, "[also failing on global.ci]")
                elif (
                    entry_kws
                    and revert_subjects
                    and any(
                        kw in r_subj
                        for kw in entry_kws
                        for r_subj in revert_subjects
                    )
                ):
                    _record_flake(entry, "[reverted on main]")
                else:
                    post_ci_actionable.append(entry)
            actionable = post_ci_actionable

    latest_status = latest_build.get("status", "UNKNOWN")
    if (
        latest_status in ("FAILURE", "INFRA_FAILURE")
        and not actionable
        and not flakes
    ):
        roller_summary = latest_build.get("summaryMarkdown", "") or ""
        first_line = first_meaningful_summary_line(roller_summary)
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
    issue_key: str | None = None
    issue_title: str | None = None
    if actionable:
        primary = actionable[0]
        safe_roller = (
            roller_name
            if re.fullmatch(r"[A-Za-z0-9_.-]+", str(roller_name))
            else "roller"
        )
        safe_builder = (
            str(primary["builder"])
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
        for cand_sig in primary.get("signatures") or ():
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
            f"{safe_roller}:{safe_cl or safe_bid}:{safe_rev or sig_token}"
        )

    return {
        "actionable": actionable,
        "flakes": flakes,
        "flake_signatures": flake_signatures,
        "issue_key": issue_key,
        "issue_title": issue_title,
    }
