#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Validates a merged review result JSON file and renders it as Markdown.

The report layout is the standard AI code review report: a decision headline,
summary, scope and evaluated skills, then findings grouped by file and
numbered in display order.

Usage:
  render_report.py --review_result_file=FILE [--workspace=DIR]
      [--min_confidence=N] [--effort=quick|standard|deep]
"""

import argparse
import itertools
import json
import os
import re
import sys

DECISIONS = {
    "approved": "✅ **Approved** — All checks pass.",
    "changes_requested": (
        "❌ **Changes requested** — See inline comments for details."
    ),
    "approved_with_suggestions": (
        "✅ **Approved with minor suggestions** — See inline comments for"
        " optional improvements."
    ),
    "not_applicable": (
        "ℹ️ **N/A** — My review skills don't apply to the contents of this"
        " change."
    ),
}

SEVERITIES = {
    "blocking": "**🚨 Blocking**",
    "suggestion": "**💡 Suggestion**",
}

EFFORTS = {
    "quick": "Quick",
    "standard": "Standard",
    "deep": "Deep",
}

_PRE_EXISTING_PREFIX_RE = re.compile(
    r"^(?:\[pre-?existing\]\s*:?\s*)+", re.IGNORECASE
)


class ReportError(ValueError):
    """Raised when the review result JSON is invalid."""


def _normalize(value):
    return str(value or "").strip().lower()


def _positive_int_or_none(finding, key, index):
    value = finding.get(key)
    if value is None:
        return None
    if not isinstance(value, int) or isinstance(value, bool) or value < 1:
        raise ReportError(f"findings[{index}].{key} must be a positive integer")
    return value


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
        raw_comment = str(finding.get("comment") or "").strip()
        if pre_existing:
            raw_comment = _PRE_EXISTING_PREFIX_RE.sub("", raw_comment).strip()
        if not raw_comment:
            raise ReportError(f"findings[{index}].comment must not be empty")
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


def _location_link(finding, workspace_root):
    path = finding["file_path"]
    target = _file_uri(path, workspace_root)
    start = finding.get("line_start")
    if start is None:
        return f"[`{path}`]({target})"
    anchor = label = f"L{start}"
    end = finding.get("line_end")
    if end is not None and end != start:
        anchor = f"{anchor}-L{end}"
        label = f"{label}-{end}"
    snippet = str(finding.get("code_snippet") or "").strip()
    if not snippet:
        return f"[**{label}**]({target}#{anchor})"
    # Keep only the first line so the link stays on one line.
    first_line = snippet.splitlines()[0].strip()
    if "\n" in snippet:
        first_line = f"{first_line} ..."
    # Use a longer backtick run than any in the snippet so it can't end the
    # code span early.
    ticks = "`" * (_longest_backtick_run(first_line) + 1)
    if ticks == "`":
        return f"[**{label}** `{first_line}`]({target}#{anchor})"
    return f"[**{label}** {ticks} {first_line} {ticks}]({target}#{anchor})"


def _render_rule(finding):
    reference = finding.get("rule_reference")
    link = finding.get("rule_link")
    if reference and link:
        return f" ([**{reference}**]({link}))"
    if reference:
        return f" (*{reference}*)"
    return ""


def _render_finding(finding, workspace_root):
    badge = SEVERITIES[finding["severity"]]
    conf = (
        f" (Confidence: {finding['confidence']}%)"
        if finding.get("confidence") is not None
        else ""
    )
    loc = (
        f" {_location_link(finding, workspace_root)}"
        if finding.get("file_path")
        else ""
    )
    comment = str(finding["comment"]).strip()
    if finding.get("pre_existing"):
        body = _PRE_EXISTING_PREFIX_RE.sub("", comment).strip()
        comment = f"[Pre-existing] {body}"
    lines = [
        f"- **#{finding['finding_id']}**{loc}",
        "",
        f"  {badge}{conf}{_render_rule(finding)}: {comment}",
    ]
    diff = finding.get("suggested_diff")
    if diff:
        # The fence must be longer than any backtick run in the diff.
        fence = "`" * max(3, _longest_backtick_run(diff) + 1)
        lines.extend(["", f"  {fence}diff"])
        lines.extend(f"  {line}" for line in diff.splitlines())
        lines.append(f"  {fence}")
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
    return f"- **Skipped Files**: {count} {noun} matched skip globs ({preview})"


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
    lines = ["## Code Review", "", DECISIONS[report["decision"]], ""]
    if report.get("summary"):
        lines.extend([report["summary"], ""])
    if report.get("scope"):
        lines.append(f"- **Scope**: {report['scope']}")
    if report.get("effort"):
        lines.append(f"- **Effort**: {EFFORTS[report['effort']]}")
    skills = report.get("agent_skills") or []
    if skills:
        lines.append(
            "- **Skills Evaluated**: " + ", ".join(f"`{s}`" for s in skills)
        )
    skipped = report.get("skipped_files") or []
    if skipped:
        lines.append(_format_skipped_files(skipped))
    lines.append("")

    # Findings without a file sort first; within a file, file-level findings
    # sort before line findings.
    ordered = sorted(
        report["findings"],
        key=lambda f: (
            f.get("file_path") or "",
            f.get("line_start") or 0,
            f.get("line_end") or 0,
        ),
    )
    for index, finding in enumerate(ordered, start=1):
        finding["finding_id"] = str(index)

    for file_path, group in itertools.groupby(
        ordered, key=lambda f: f.get("file_path") or ""
    ):
        lines.extend(["---", ""])
        if file_path:
            uri = _file_uri(file_path, workspace_root)
            lines.extend([f"### [`{file_path}`]({uri})", ""])
        for finding in group:
            lines.extend(_render_finding(finding, workspace_root))

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
