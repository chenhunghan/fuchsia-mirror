#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Deterministic compliance auditor for /autoda-fix v1.2.0 sessions.

Enforces Gate 1 (pre-fix, end of Phase 2) and Gate 2 (pre-commit, Phase 4)
invariants on `bug_spec.md` and `bug_devlog.md` before `session-auditor` runs:

Gate 1 (`--gate pre-fix`):
  1. Valid YAML frontmatter in `bug_spec.md` (`skill_version`,
     `verification_mode`, `adversarial_review`).
  2. Strict `verification_mode` value ("Mode A", "Mode B", or "Mode C") with
     zero parenthetical fallback smuggling (e.g. "Mode A (Mode C fallback)").
  3. Presence of required sections in `bug_spec.md`:
     - `## 1. Symptoms & Evidence`
     - `## 2. Candidate Hypotheses & Discriminating Experiments`
     - `## 3. Validated Root Cause`
     - `## 3b. Cross-Subagent Contradiction & Reconciliation Ledger`
     - `## 3c. In-Driver & End-to-End Invariant Trace` (covering Axis 1,
       Axis 2, and Axis 3)
  4. When `verification_mode` is "Mode A" or "Mode B", checks that a hardware
     target is documented or reachable via `ffx target list` (unless
     `--skip-live-target-check` is set for unit testing).

Gate 2 (`--gate pre-commit`):
  1. All Gate 1 checks.
  2. Valid YAML frontmatter in `bug_devlog.md` matching `bug_spec.md`
     `verification_mode`.
  3. For "Mode A" or "Mode B", verifies `bug_devlog.md` contains concrete
     hardware verification evidence (e.g. `driver-lab`, `ffx log`, `ffx target`,
     or `ffx driver`) rather than only `fx build` or mock tests.
  4. When `adversarial_review: "enabled"`, verifies `bug_devlog.md` records
     Phase 3.5 Adversarial Review rounds and verdict (`VERDICT: PASS` or
     max-round escalation).
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Tuple

_ALLOWED_MODES = {"Mode A", "Mode B", "Mode C"}
_ALLOWED_ADVERSARIAL = {"enabled", "disabled"}

_REQUIRED_SPEC_SECTIONS = [
    (
        "## 1. Symptoms & Evidence",
        r"(?m)^##\s+1\.\s+Symptoms\s+&\s+Evidence\b",
    ),
    (
        "## 2. Candidate Hypotheses & Discriminating Experiments",
        r"(?m)^##\s+2\.\s+Candidate\s+Hypotheses\b",
    ),
    (
        "## 3. Validated Root Cause",
        r"(?m)^##\s+3\.\s+Validated\s+Root\s+Cause\b",
    ),
    (
        "## 3b. Cross-Subagent Contradiction & Reconciliation Ledger",
        r"(?m)^##\s+3b\.\s+Cross-Subagent\s+Contradiction\b",
    ),
    (
        "## 3c. In-Driver & End-to-End Invariant Trace",
        r"(?m)^##\s+3c\.\s+In-Driver\s+&\s+End-to-End\s+Invariant\s+Trace\b",
    ),
]


def parse_frontmatter(text: str) -> Dict[str, str]:
    """Parses simple key: value YAML frontmatter from markdown text."""
    m = re.match(r"\A---\s*\n(.*?)\n---\s*(?:\n|\Z)", text, re.DOTALL)
    if not m:
        return {}
    fm: Dict[str, str] = {}
    for line in m.group(1).splitlines():
        line = line.strip()
        if not line or line.startswith("#") or ":" not in line:
            continue
        key, val = line.split(":", 1)
        key = key.strip()
        val = val.strip()
        if (val.startswith('"') and val.endswith('"')) or (
            val.startswith("'") and val.endswith("'")
        ):
            val = val[1:-1]
        fm[key] = val
    return fm


def extract_section_body(text: str, heading_regex: str) -> str:
    """Extracts the markdown body under a `## ...` heading until the next `## `."""
    m = re.search(heading_regex, text)
    if not m:
        return ""
    start = m.end()
    next_heading = re.search(r"(?m)^##\s+", text[start:])
    if next_heading:
        return text[start : start + next_heading.start()].strip()
    return text[start:].strip()


def check_live_target() -> Tuple[bool, str]:
    """Checks `ffx target list` for at least one reachable target device."""
    try:
        proc = subprocess.run(
            ["ffx", "target", "list"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        out = (proc.stdout or "") + (proc.stderr or "")
        if proc.returncode != 0:
            return False, f"`ffx target list` exited with {proc.returncode}"
        if "No devices found" in out or not out.strip():
            return False, "`ffx target list` reported no devices found"
        return True, "Live target visible via `ffx target list`"
    except (OSError, subprocess.SubprocessError) as exc:
        return False, f"Unable to run `ffx target list`: {exc}"


def audit_pre_fix(
    bug_spec_path: Path,
    bug_devlog_path: Path,
    skip_live_target_check: bool = False,
) -> List[str]:
    """Runs Gate 1 (`pre-fix`) compliance checks."""
    violations: List[str] = []

    if not bug_spec_path.is_file():
        violations.append(
            f"[GATE1_MISSING_BUG_SPEC] `{bug_spec_path}` does not exist."
        )
        return violations

    spec_text = bug_spec_path.read_text(encoding="utf-8", errors="replace")
    fm = parse_frontmatter(spec_text)
    if not fm:
        violations.append(
            "[GATE1_MISSING_FRONTMATTER] `bug_spec.md` is missing YAML "
            "frontmatter (`--- ... ---`)."
        )
    else:
        mode = fm.get("verification_mode", "")
        if mode not in _ALLOWED_MODES:
            violations.append(
                "[GATE1_INVALID_VERIFICATION_MODE] `bug_spec.md` frontmatter "
                f"`verification_mode` must be strictly one of "
                f"{sorted(_ALLOWED_MODES)}, got: {mode!r}. Parenthetical "
                "Mode C fallbacks or hybrid labels are forbidden."
            )
        adv = fm.get("adversarial_review", "")
        if adv not in _ALLOWED_ADVERSARIAL:
            violations.append(
                "[GATE1_MISSING_ADVERSARIAL_CHOICE] `bug_spec.md` frontmatter "
                "`adversarial_review` must be 'enabled' or 'disabled', "
                f"got: {adv!r}."
            )

    for label, pattern in _REQUIRED_SPEC_SECTIONS:
        body = extract_section_body(spec_text, pattern)
        if not re.search(pattern, spec_text):
            violations.append(
                f"[GATE1_MISSING_SECTION] `bug_spec.md` is missing required "
                f"heading `{label}`."
            )
        elif len(body) < 20:
            violations.append(
                f"[GATE1_EMPTY_SECTION] `bug_spec.md` section `{label}` is "
                "empty or placeholder-only."
            )

    # Check 3c explicitly covers Axis 1, Axis 2, and Axis 3
    sec_3c = extract_section_body(
        spec_text,
        r"(?m)^##\s+3c\.\s+In-Driver\s+&\s+End-to-End\s+Invariant\s+Trace\b",
    )
    if sec_3c:
        for axis_label, axis_re in [
            ("Axis 1 (Sibling Primitive Sweep)", r"(?i)Axis\s*1|Sibling"),
            ("Axis 2 (State-Machine / Dataflow)", r"(?i)Axis\s*2|Dataflow"),
            ("Axis 3 (Caller/Callee & Litmus Test)", r"(?i)Axis\s*3|Litmus"),
        ]:
            if not re.search(axis_re, sec_3c):
                violations.append(
                    f"[GATE1_INCOMPLETE_3C_TRACE] `## 3c` in `bug_spec.md` "
                    f"does not address {axis_label}."
                )

    # Check live hardware target if Mode A or Mode B
    mode = fm.get("verification_mode", "")
    if mode in ("Mode A", "Mode B") and not skip_live_target_check:
        devlog_text = (
            bug_devlog_path.read_text(encoding="utf-8", errors="replace")
            if bug_devlog_path.is_file()
            else ""
        )
        has_devlog_hw = bool(
            re.search(
                r"(?i)(ffx\s+target\s+list|driver-lab|RCS\s*:\s*Y|Target\s+attached)",
                devlog_text,
            )
        )
        ok, reason = check_live_target()
        if not ok and not has_devlog_hw:
            violations.append(
                f"[GATE1_MISSING_HARDWARE_TARGET] `{mode}` requires an "
                f"attached hardware target, but {reason} and `bug_devlog.md` "
                "has no verified target attachment record."
            )

    return violations


def audit_pre_commit(
    bug_spec_path: Path,
    bug_devlog_path: Path,
    skip_live_target_check: bool = False,
) -> List[str]:
    """Runs Gate 2 (`pre-commit`) compliance checks."""
    violations = audit_pre_fix(
        bug_spec_path,
        bug_devlog_path,
        skip_live_target_check=skip_live_target_check,
    )

    if not bug_devlog_path.is_file():
        violations.append(
            f"[GATE2_MISSING_BUG_DEVLOG] `{bug_devlog_path}` does not exist."
        )
        return violations

    spec_text = (
        bug_spec_path.read_text(encoding="utf-8", errors="replace")
        if bug_spec_path.is_file()
        else ""
    )
    devlog_text = bug_devlog_path.read_text(encoding="utf-8", errors="replace")

    spec_fm = parse_frontmatter(spec_text)
    devlog_fm = parse_frontmatter(devlog_text)
    if not devlog_fm:
        violations.append(
            "[GATE2_MISSING_DEVLOG_FRONTMATTER] `bug_devlog.md` is missing "
            "YAML frontmatter (`--- ... ---`)."
        )
    else:
        spec_mode = spec_fm.get("verification_mode", "")
        devlog_mode = devlog_fm.get("verification_mode", "")
        if devlog_mode not in _ALLOWED_MODES:
            violations.append(
                "[GATE2_INVALID_DEVLOG_MODE] `bug_devlog.md` frontmatter "
                f"`verification_mode` must be strictly one of "
                f"{sorted(_ALLOWED_MODES)}, got: {devlog_mode!r}."
            )
        elif spec_mode and devlog_mode != spec_mode:
            violations.append(
                "[GATE2_MODE_DRIFT] `verification_mode` in `bug_devlog.md` "
                f"({devlog_mode!r}) does not match `bug_spec.md` "
                f"({spec_mode!r}). Silent mode downgrade is forbidden."
            )

    mode = spec_fm.get("verification_mode", "")
    if mode in ("Mode A", "Mode B"):
        has_hw_verification = bool(
            re.search(
                r"(?i)(driver-lab|ffx\s+driver\s+(restart|disable|register)|"
                r"ffx\s+log|ffx\s+test|fx\s+test\s+[^\n]*--device)",
                devlog_text,
            )
        )
        if not has_hw_verification:
            violations.append(
                f"[GATE2_MISSING_HITL_EVIDENCE] `{mode}` was approved, but "
                "`bug_devlog.md` contains no hardware verification evidence "
                "(`driver-lab`, `ffx driver restart`, `ffx log`, or on-device "
                "test execution). Compile-only or host mock tests cannot "
                f"satisfy `{mode}`."
            )

    if spec_fm.get("adversarial_review") == "enabled":
        has_adv_log = bool(
            re.search(
                r"(?i)(Adversarial\s+Review|adversarial-reviewer|"
                r"VERDICT\s*:\s*(PASS|OBJECTIONS_FOUND))",
                devlog_text,
            )
        )
        if not has_adv_log:
            violations.append(
                "[GATE2_MISSING_ADVERSARIAL_REVIEW] `adversarial_review` is "
                "'enabled' in `bug_spec.md`, but `bug_devlog.md` contains no "
                "record of Phase 3.5 Adversarial Review execution or verdict."
            )

    return violations


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Deterministic compliance auditor for /autoda-fix v1.2.0."
    )
    parser.add_argument(
        "--gate",
        required=True,
        choices=["pre-fix", "pre-commit"],
        help="Audit gate to enforce ('pre-fix' for Gate 1, 'pre-commit' for Gate 2).",
    )
    parser.add_argument(
        "--bug-spec",
        required=True,
        help="Path to bug_spec.md artifact.",
    )
    parser.add_argument(
        "--bug-devlog",
        required=True,
        help="Path to bug_devlog.md artifact.",
    )
    parser.add_argument(
        "--skip-live-target-check",
        action="store_true",
        help="Skip live `ffx target list` subprocess check (rely on artifact text).",
    )
    args = parser.parse_args()

    spec_path = Path(args.bug_spec)
    devlog_path = Path(args.bug_devlog)

    if args.gate == "pre-fix":
        violations = audit_pre_fix(
            spec_path,
            devlog_path,
            skip_live_target_check=args.skip_live_target_check,
        )
    else:
        violations = audit_pre_commit(
            spec_path,
            devlog_path,
            skip_live_target_check=args.skip_live_target_check,
        )

    result = {
        "gate": args.gate,
        "status": "PASS" if not violations else "FAIL",
        "violation_count": len(violations),
        "violations": violations,
    }
    print(json.dumps(result, indent=2))
    return 0 if not violations else 1


if __name__ == "__main__":
    sys.exit(main())
