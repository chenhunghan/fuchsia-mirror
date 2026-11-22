#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Validates a merged review result JSON file and renders it as Markdown.

The report layout uses progressive disclosure: a verdict headline, brief
summary, metadata bullets, and findings grouped by severity (blocking issues
before suggestions) and numbered sequentially in display order.

Usage:
  render_report.py --review_result_file=FILE [--workspace=DIR]
      [--min_confidence=N] [--effort=quick|standard|deep|simplify]
"""

import argparse
import html
import itertools
import json
import os
import re
import sys
import types

DECISIONS = {
    "approved": "🟢 **Approved**",
    "changes_requested": "❌ **Changes requested**",
    "approved_with_suggestions": "🟢 **Approved with minor suggestions**",
    "not_applicable": (
        "ℹ️ **Not applicable** (no review skills apply to this change)"
    ),
}

SEVERITIES = {
    "blocking": "**🚨 Blocking**",
    "suggestion": "**💡 Suggestion**",
}

_SEVERITY_SECTIONS = types.MappingProxyType(
    {
        "blocking": "### Blocking issues",
        "suggestion": "### Suggestions",
    }
)

EFFORTS = {
    "quick": "Quick",
    "standard": "Standard",
    "deep": "Deep",
    "simplify": "Simplify",
}

_SHORT_FILENAME_THRESHOLD = 24
_MAX_PACKAGE_DEPTH = 4
_DEFAULT_TRACE_TITLE = "Supporting details"
_PACKAGE_ROOT_MARKERS = (
    "BUILD.gn",
    "BUILD.bazel",
    "BUILD",
    "Cargo.toml",
    "SKILL.md",
)
_STRUCTURED_KEYS = (
    "headline",
    "why",
    "details",
    "action",
    "trace",
    "trace_title",
)

_PRE_EXISTING_PREFIX_RE = re.compile(
    r"^(?:\[pre-?existing\]\s*:?\s*)+", re.IGNORECASE
)
_BULLET_PREFIX_RE = re.compile(r"^[-*+]\s+")
_ORDERED_PREFIX_RE = re.compile(r"^(\d+[.)]\s+)(?=\S)")


class ReportError(ValueError):
    """Raised when the review result JSON is invalid."""


def _normalize(value):
    return str(value or "").strip().lower()


def _non_negative_int(value, label):
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ReportError(f"{label} must be a non-negative integer")
    return value


def _positive_int_or_none(finding, key, index):
    value = finding.get(key)
    if value is None:
        return None
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ReportError(f"findings[{index}].{key} must be a positive integer")
    return value


def _clean_required_text(
    value,
    label,
    strip_pre_existing=False,
    single_line=False,
):
    if value is None:
        raise ReportError(f"{label} must not be empty")
    if not isinstance(value, str):
        raise ReportError(f"{label} must be a string")
    text = " ".join(value.split()) if single_line else value.strip()
    if strip_pre_existing:
        text = _PRE_EXISTING_PREFIX_RE.sub("", text)
        text = " ".join(text.split()) if single_line else text.strip()
    if not text:
        raise ReportError(f"{label} must not be empty")
    return text


def _clean_optional_str(finding, key, index, single_line=False):
    value = finding.get(key)
    if value is None:
        finding.pop(key, None)
        return None
    if not isinstance(value, str):
        raise ReportError(f"findings[{index}].{key} must be a string")
    cleaned = " ".join(value.split()) if single_line else value.strip()
    if not cleaned:
        finding.pop(key, None)
        return None
    finding[key] = cleaned
    return cleaned


def _clean_string_list(finding, key, index):
    items = finding.get(key)
    if items is None:
        return None
    if not isinstance(items, list):
        raise ReportError(f"findings[{index}].{key} must be a list of strings")
    cleaned = []
    for item_index, item in enumerate(items):
        if not isinstance(item, str) or not item.strip():
            raise ReportError(
                f"findings[{index}].{key}[{item_index}] must be a non-empty"
                " string"
            )
        cleaned.append(item.strip())
    return cleaned


def _validate_challenger_counts(payload):
    kept = payload.get("challenger_kept")
    total = payload.get("challenger_total")
    if kept is None and total is None:
        return
    if kept is None or total is None:
        raise ReportError(
            "challenger_kept and challenger_total must be provided together"
        )
    _non_negative_int(kept, "challenger_kept")
    _non_negative_int(total, "challenger_total")
    if kept > total:
        raise ReportError("challenger_kept must be <= challenger_total")


def _validate_finding_body(finding, index, pre_existing):
    if any(key in finding for key in _STRUCTURED_KEYS):
        finding["headline"] = _clean_required_text(
            finding.get("headline"),
            f"findings[{index}].headline",
            strip_pre_existing=pre_existing,
            single_line=True,
        )
        finding["why"] = _clean_required_text(
            finding.get("why"), f"findings[{index}].why"
        )
        details = _clean_string_list(finding, "details", index)
        if details is not None:
            finding["details"] = details
        _clean_optional_str(finding, "action", index)
        trace = _clean_string_list(finding, "trace", index)
        if trace is not None:
            finding["trace"] = trace
        _clean_optional_str(finding, "trace_title", index, single_line=True)
        return

    finding["comment"] = _clean_required_text(
        finding.get("comment"),
        f"findings[{index}].comment",
        strip_pre_existing=pre_existing,
    )


def validate(payload):
    """Validates `payload` in place and returns it.

    Normalizes `decision`, optional `effort`, and each finding's `severity` to
    lower case, and downgrades any `pre_existing: true` finding to `suggestion`.

    Raises:
      ReportError: if a required field is missing or invalid.
    """
    if not isinstance(payload, dict):
        raise ReportError("review result must be a JSON object")
    decision = _normalize(payload.get("decision"))
    if decision not in DECISIONS:
        raise ReportError(
            f"unsupported decision {payload.get('decision')!r}; expected one"
            f" of: {', '.join(sorted(DECISIONS))}"
        )
    payload["decision"] = decision
    effort = payload.get("effort")
    if effort is not None:
        if not isinstance(effort, str):
            raise ReportError(
                f"unsupported effort {effort!r}; expected one of:"
                f" {', '.join(sorted(EFFORTS))}"
            )
        norm_effort = _normalize(effort)
        if not norm_effort:
            payload.pop("effort", None)
        elif norm_effort not in EFFORTS:
            raise ReportError(
                f"unsupported effort {effort!r}; expected one of:"
                f" {', '.join(sorted(EFFORTS))}"
            )
        else:
            payload["effort"] = norm_effort
    _validate_challenger_counts(payload)
    findings = payload.get("findings")
    if findings is None:
        findings = []
    if not isinstance(findings, list):
        raise ReportError("findings must be a list")
    for index, finding in enumerate(findings):
        if not isinstance(finding, dict):
            raise ReportError(f"findings[{index}] must be an object")
        severity = _normalize(finding.get("severity"))
        if severity not in SEVERITIES:
            raise ReportError(
                f"findings[{index}].severity must be one of:"
                f" {', '.join(sorted(SEVERITIES))}"
            )
        pre_existing = finding.get("pre_existing")
        if pre_existing is not None and not isinstance(pre_existing, bool):
            raise ReportError(
                f"findings[{index}].pre_existing must be a boolean"
            )
        finding["severity"] = "suggestion" if pre_existing else severity
        _validate_finding_body(finding, index, bool(pre_existing))
        if "file_path" in finding:
            _clean_optional_str(finding, "file_path", index, single_line=True)
        confidence = finding.get("confidence")
        if confidence is not None:
            if (
                not isinstance(confidence, int)
                or isinstance(confidence, bool)
                or not 0 <= confidence <= 100
            ):
                raise ReportError(
                    f"findings[{index}].confidence must be an integer between"
                    " 0 and 100"
                )
        start = _positive_int_or_none(finding, "line_start", index)
        end = _positive_int_or_none(finding, "line_end", index)
        if end is not None and (start is None or end < start):
            raise ReportError(
                f"findings[{index}].line_end must be >= line_start"
            )
    skills = payload.get("agent_skills")
    if skills is not None:
        if not isinstance(skills, list) or not all(
            isinstance(s, str) for s in skills
        ):
            raise ReportError("agent_skills must be a list of strings")
    skipped = payload.get("skipped_files")
    if skipped is not None:
        if not isinstance(skipped, list) or not all(
            isinstance(p, str) for p in skipped
        ):
            raise ReportError("skipped_files must be a list of strings")
    payload["findings"] = findings
    return payload


def _longest_backtick_run(text):
    return max((len(m) for m in re.findall(r"`+", text)), default=0)


def _file_uri(file_path, workspace_root):
    full_path = os.path.join(workspace_root, file_path.lstrip("/"))
    return f"file:///{full_path.lstrip('/')}"


def _initial_suffix_index(parts, workspace_root):
    if len(parts) <= 1:
        return 0
    if workspace_root and os.path.isdir(workspace_root):
        min_idx = max(0, len(parts) - _MAX_PACKAGE_DEPTH)
        for idx in range(len(parts) - 2, min_idx - 1, -1):
            candidate_dir = os.path.join(workspace_root, *parts[: idx + 1])
            if any(
                os.path.isfile(os.path.join(candidate_dir, marker))
                for marker in _PACKAGE_ROOT_MARKERS
            ):
                return idx
    if len(parts[-1]) >= _SHORT_FILENAME_THRESHOLD:
        return len(parts) - 1
    return len(parts) - 2


def _collect_distinct_part_tuples(report):
    seen = {}
    for finding in report.get("findings") or ():
        path = finding.get("file_path")
        if path is not None:
            parts = tuple(p for p in path.split("/") if p)
            if parts:
                seen[parts] = None
    for entry in report.get("reviewed_files") or ():
        if isinstance(entry, dict) and isinstance(
            raw := entry.get("file_path"), str
        ):
            cleaned = " ".join(raw.split())
            parts = tuple(p for p in cleaned.split("/") if p)
            if parts:
                seen[parts] = None
    return tuple(seen)


def _build_short_paths(report, workspace_root):
    indices = {
        parts: _initial_suffix_index(parts, workspace_root)
        for parts in _collect_distinct_part_tuples(report)
    }
    while True:
        by_suffix = {}
        for parts, idx in indices.items():
            by_suffix.setdefault(parts[idx:], []).append(parts)
        progressed = False
        for group in by_suffix.values():
            if len(group) > 1:
                for parts in group:
                    if indices[parts] > 0:
                        indices[parts] -= 1
                        progressed = True
        if not progressed:
            break
    return {parts: "/".join(parts[idx:]) for parts, idx in indices.items()}


def _short_location_link(finding, workspace_root, short_paths):
    path = finding["file_path"]
    target = _file_uri(path, workspace_root)
    parts = tuple(part for part in path.split("/") if part)
    short_path = short_paths.get(parts, path) if parts else path
    start = finding.get("line_start")
    if start is None:
        return f"[{short_path}]({target})"
    anchor = f"L{start}"
    label = f"{short_path}:{start}"
    end = finding.get("line_end")
    if end is not None and end != start:
        anchor = f"{anchor}-L{end}"
        label = f"{label}-{end}"
    return f"[{label}]({target}#{anchor})"


def _render_rule(finding):
    reference = finding.get("rule_reference")
    link = finding.get("rule_link")
    if reference and link:
        return f" ([**{reference}**]({link}))"
    if reference:
        return f" (*{reference}*)"
    return ""


def _render_diff_block(diff, indent):
    # The fence must be longer than any backtick run in the diff.
    fence = "`" * max(3, _longest_backtick_run(diff) + 1)
    lines = ["", f"{indent}{fence}diff"]
    lines.extend(f"{indent}{line}" for line in diff.splitlines())
    lines.append(f"{indent}{fence}")
    return lines


def _strip_bullet_prefix(text):
    raw_lines = text.splitlines()
    if (rest := _BULLET_PREFIX_RE.sub("", raw_lines[0], count=1)).strip():
        raw_lines[0] = rest
    return "\n".join(raw_lines)


def _render_list_item(item_indent, marker, text):
    cont_indent = f"{item_indent}{' ' * len(marker)}"
    raw_lines = text.splitlines()
    lines = [f"{item_indent}{marker}{raw_lines[0]}"]
    for line in raw_lines[1:]:
        lines.append(f"{cont_indent}{line}" if line.strip() else "")
    return lines


def _render_bullet(bullet_indent, text):
    return _render_list_item(bullet_indent, "- ", text)


def _render_trace_items(indent, trace):
    lines = []
    prev_ordered = None
    for item in trace:
        raw_lines = item.splitlines()
        match = _ORDERED_PREFIX_RE.match(raw_lines[0])
        is_ordered = match is not None
        if prev_ordered is not None and prev_ordered != is_ordered:
            lines.append("")
        if match is not None:
            marker = match.group(1)
            body = "\n".join((raw_lines[0][len(marker) :], *raw_lines[1:]))
            lines.extend(_render_list_item(indent, marker, body))
        else:
            lines.extend(_render_bullet(indent, _strip_bullet_prefix(item)))
        prev_ordered = is_ordered
    return lines


def _render_finding(finding, workspace_root, short_paths):
    prefix = f"{finding['finding_id']}. "
    indent = " " * len(prefix)
    badge = SEVERITIES[finding["severity"]]
    pre = " [Pre-existing]" if finding.get("pre_existing") else ""
    conf = (
        f" (Confidence: {finding['confidence']}%)"
        if finding.get("confidence") is not None
        else ""
    )
    provenance = f"{badge}{pre}{conf}{_render_rule(finding)}"
    loc = (
        _short_location_link(finding, workspace_root, short_paths)
        if finding.get("file_path") is not None
        else ""
    )

    if finding.get("headline") is not None:
        header = (
            f"{prefix}**{finding['headline']}** ({loc})"
            if loc
            else f"{prefix}**{finding['headline']}**"
        )
        lines = [header, "", f"{indent}{provenance}", ""]
        lines.extend(
            _render_bullet(indent, f"**Why it matters:** {finding['why']}")
        )
        for detail in finding.get("details") or ():
            lines.extend(
                _render_bullet(f"{indent}  ", _strip_bullet_prefix(detail))
            )
        if (action := finding.get("action")) is not None:
            lines.extend(_render_bullet(indent, f"**Action:** {action}"))
        if (diff := finding.get("suggested_diff")) is not None and diff:
            lines.extend(_render_diff_block(diff, indent))
        trace = finding.get("trace")
        if trace:
            raw_title = finding.get("trace_title")
            trace_title = html.escape(
                raw_title if raw_title is not None else _DEFAULT_TRACE_TITLE,
                quote=False,
            )
            lines.extend(
                [
                    "",
                    f"{indent}<details>",
                    f"{indent}<summary>{trace_title}</summary>",
                    "",
                ]
            )
            lines.extend(_render_trace_items(indent, trace))
            lines.append(f"{indent}</details>")
        lines.append("")
        return lines

    lines = (
        [f"{prefix}{loc}", "", f"{indent}{provenance}", ""]
        if loc
        else [f"{prefix}{provenance}", ""]
    )
    for line in finding["comment"].splitlines():
        lines.append(f"{indent}{line}" if line.strip() else "")
    diff = finding.get("suggested_diff")
    if diff:
        lines.extend(_render_diff_block(diff, indent))
    lines.append("")
    return lines


def _recompute_decision(current_decision, findings):
    if current_decision == "not_applicable":
        return current_decision
    if any(f["severity"] == "blocking" for f in findings):
        return "changes_requested"
    if findings:
        return "approved_with_suggestions"
    return "approved"


def _format_decision(decision, n_blocking, n_suggestions):
    base = DECISIONS[decision]
    if decision in ("changes_requested", "approved_with_suggestions"):
        noun = "suggestion" if n_suggestions == 1 else "suggestions"
        return f"{base} ({n_blocking} blocking, {n_suggestions} {noun})"
    return base


def _meets_min_confidence(finding, min_confidence, require_score):
    score = finding.get("confidence")
    if score is None:
        return not require_score
    return score >= min_confidence


def _format_skipped_files(skipped):
    count = len(skipped)
    noun = "file" if count == 1 else "files"
    preview = ", ".join(f"`{p}`" for p in skipped[:5])
    if count > 5:
        preview = f"{preview}, +{count - 5} more"
    return f"- **Skipped files**: {count} {noun} matched skip globs ({preview})"


def render(payload, workspace_root="", min_confidence=None, effort=None):
    """Validates `payload`, numbers its findings, and returns Markdown.

    Sets sequential `finding_id`s ("1", "2", ...) on the findings in display
    order so follow-up fixes can refer to the numbers shown in the report.
    """
    report = validate(payload)
    if effort is not None:
        norm_effort = _normalize(effort)
        if norm_effort not in EFFORTS:
            raise ReportError(
                f"unsupported effort {effort!r}; expected one of:"
                f" {', '.join(sorted(EFFORTS))}"
            )
        report["effort"] = norm_effort
    is_quick = report.get("effort") == "quick"
    if is_quick:
        report["findings"] = [
            f for f in report["findings"] if f["severity"] == "blocking"
        ]
    elif report.get("effort") == "simplify":
        for finding in report["findings"]:
            finding["severity"] = "suggestion"
    if min_confidence is not None:
        if not 0 <= min_confidence <= 100:
            raise ReportError("min_confidence must be between 0 and 100")
        report["findings"] = [
            f
            for f in report["findings"]
            if _meets_min_confidence(f, min_confidence, require_score=is_quick)
        ]
    report["decision"] = _recompute_decision(
        report["decision"], report["findings"]
    )
    # Blocking findings always appear before suggestions; within each group,
    # sort by file path and line range.
    ordered = sorted(
        report["findings"],
        key=lambda f: (
            0 if f["severity"] == "blocking" else 1,
            f.get("file_path") or "",
            f.get("line_start") or 0,
            f.get("line_end") or 0,
        ),
    )
    for index, finding in enumerate(ordered, start=1):
        finding["finding_id"] = str(index)

    n_blocking = sum(1 for f in ordered if f["severity"] == "blocking")
    n_suggestions = len(ordered) - n_blocking

    lines = [
        "## Code review",
        "",
        _format_decision(report["decision"], n_blocking, n_suggestions),
        "",
    ]
    summary = str(report.get("summary") or "").strip()
    if summary:
        lines.extend([summary, ""])
    meta_lines = []
    scope = " ".join(str(report.get("scope") or "").split())
    if scope:
        meta_lines.append(f"- **Scope**: {scope}")
    if report.get("effort"):
        effort_line = f"- **Effort**: {EFFORTS[report['effort']]}"
        if report.get("challenger_total") and not is_quick:
            kept = report["challenger_kept"]
            total = report["challenger_total"]
            effort_line += (
                f" (challenger kept {kept} of {total} Round-1 draft findings)"
            )
        meta_lines.append(effort_line)
    skills = report.get("agent_skills") or []
    if skills:
        meta_lines.append("- **Skills evaluated**: " + ", ".join(skills))
    skipped = report.get("skipped_files") or []
    if skipped:
        meta_lines.append(_format_skipped_files(skipped))
    if meta_lines:
        lines.extend(meta_lines)
        lines.append("")

    short_paths = _build_short_paths(report, workspace_root)
    for severity, group in itertools.groupby(
        ordered, key=lambda f: f["severity"]
    ):
        lines.extend(["---", "", _SEVERITY_SECTIONS[severity], ""])
        for finding in group:
            lines.extend(_render_finding(finding, workspace_root, short_paths))

    report["findings"] = ordered
    return "\n".join(lines).rstrip() + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--review_result_file", required=True)
    parser.add_argument(
        "--workspace",
        default="",
        help="Repository root used to build file:// links (default: none).",
    )
    parser.add_argument(
        "--min_confidence",
        type=int,
        default=None,
        help="Drop findings whose confidence is below this 0-100 threshold.",
    )
    parser.add_argument(
        "--effort",
        type=_normalize,
        choices=tuple(EFFORTS),
        default=None,
        help="Set or override the effort level shown in the report header.",
    )
    args = parser.parse_args(argv)
    with open(args.review_result_file, encoding="utf-8") as f:
        payload = json.load(f)
    try:
        markdown = render(
            payload,
            os.path.abspath(args.workspace) if args.workspace else "",
            min_confidence=args.min_confidence,
            effort=args.effort,
        )
    except ReportError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    # Write back the numbered findings so fixes can refer to finding IDs.
    with open(args.review_result_file, "w", encoding="utf-8") as f:
        json.dump(payload, f, indent=2, ensure_ascii=False)
        f.write("\n")
    sys.stdout.write(markdown)
    return 0


if __name__ == "__main__":
    sys.exit(main())
