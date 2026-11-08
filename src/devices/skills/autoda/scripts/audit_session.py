#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Deterministic compliance auditor for AutoDA sessions.

Shared by the AutoDA entry-point skills. Select the workflow with
`--workflow fix` (default, `/autoda-fix`) or `--workflow recon`
(`/autoda-recon`). Each workflow has two gates that run before the
`session-auditor` subagent.

Fix workflow (`bug_spec.md` / `bug_devlog.md`):

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

Recon workflow (`recon_spec.md` / `recon_devlog.md`):

Static gate (`--gate static`, end of static analysis):
  1. Valid YAML frontmatter in both artifacts with a shared `system_id`,
     a non-empty `target_scope`, `hardware_verification` of "enabled" or
     "disabled", an `outcome` from the allowed set, and `claims_total` /
     `claims_contradicted` (plus `claims_verified` when present) matching the
     parsed claims under `## 3. Claims`.
  2. Presence of the seven required `recon_spec.md` sections.
  3. Every claim block (`- **C<n>** ...`) under `## 3. Claims` carries a
     confidence tier, a status, a concrete non-placeholder `Applies-to:` scope,
     and a `Source:` anchor (code path@commit:lines, bug comment, URL, or
     document/URI locator) followed by a `-- "..."` verbatim quote. A claim may
     only be `[VERIFIED]` with the `[HW_VERIFIED]` tier and an `Evidence:` line
     naming a `sha256:` manifest hash and a `boot_id`.
  4. Status markers for the reference lookups are recorded
     (`VERIFIED_FROM_SOURCE` or `BLOCKED_MISSING_LINUX_SOURCE`, and
     `VERIFIED_FROM_DATASHEET` or `UNAVAILABLE_NO_DATASHEET_FOUND`).
  5. No simulated or synthetic values in the claims or evidence sections.
  6. When `--repo-root` is provided, `git status --porcelain` is clean.

Verified gate (`--gate verified`, after hardware verification):
  1. All static gate checks (at `--repo-root`, dirty state is allowed only when
     `recon_devlog.md` records a work-in-progress enablement patch).
  2. `hardware_verification` is "enabled" and at least one claim is
     `[VERIFIED]`.
  3. `recon_devlog.md` records driver-lab evidence (`manifest.json`,
     `target-audit.jsonl`, `boot_id`), and every manifest hash cited by a
     `[VERIFIED]` claim appears in the devlog.
  4. A hardware target is documented or reachable (unless
     `--skip-live-target-check` is set).
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Optional, Tuple

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

_RECON_TIERS = {
    "HW_VERIFIED",
    "DATASHEET",
    "LINUX_DERIVED",
    "BUG_INFERRED",
    "BINARY_INFERRED",
    "AGENT_INFERRED",
}
_RECON_STATUSES = {"UNVERIFIED", "VERIFIED", "CONTRADICTED"}
_RECON_OUTCOMES = {
    "IN_PROGRESS",
    "STATIC_COMPLETE",
    "HARDWARE_VERIFIED",
    "BLOCKED",
}
_RECON_HW_VERIFICATION = {"enabled", "disabled"}

_RECON_CLAIMS_HEADING = r"(?m)^##\s+3\.\s+Claims\b"
_RECON_EVIDENCE_HEADING = r"(?m)^##\s+7\.\s+Hardware\s+Verification\b"
_REQUIRED_RECON_SECTIONS = [
    (
        "## 1. System Identity & Scope",
        r"(?m)^##\s+1\.\s+System\s+Identity\s+&\s+Scope\b",
    ),
    (
        "## 2. Reference Sources & Provenance",
        r"(?m)^##\s+2\.\s+Reference\s+Sources\b",
    ),
    ("## 3. Claims", _RECON_CLAIMS_HEADING),
    (
        "## 4. Dependencies & Bring-Up Order",
        r"(?m)^##\s+4\.\s+Dependencies\b",
    ),
    (
        "## 5. Cross-Subagent Contradiction & Reconciliation Ledger",
        r"(?m)^##\s+5\.\s+Cross-Subagent\s+Contradiction\b",
    ),
    (
        "## 6. Gaps & Blocked Sources",
        r"(?m)^##\s+6\.\s+Gaps\b",
    ),
    (
        "## 7. Hardware Verification Plan & Evidence",
        _RECON_EVIDENCE_HEADING,
    ),
]

_CLAIM_HEADER_RE = re.compile(r"(?m)^-\s+\*\*C(\d+)\*\*(.*)$")
_CLAIM_FIELD_RE = r"(?im)^\s*-\s*\*\*{field}:\*\*\s*(.*)$"
_PLACEHOLDER_SCOPE_RE = re.compile(
    r"(?i)^\s*("
    r"tbd|todo|unknown|unspecified|none|null|n/?a|pending|"
    r"scope|target[_ -]?scope|applies[_ -]?to|default|all|any|"
    r"same(?:\s+as\s+above)?|see\s+above|ditto|"
    r"<[^>\n]+>|\{[^}\n]+\}|\[[^\]\n]+\]|\?+|-+"
    r")?\s*$"
)
_QUOTED_SPAN_RE = re.compile(r'"[^"\n]*"|\u201c[^\u201d\n]*\u201d|`[^`\n]*`')
_SOURCE_ANCHOR_RE = re.compile(
    r"(@|/"
    r"|(?:https?|file)://\S+"
    r"|\b(?:doc|gdoc|drive|urn|git|cs|cl|bug):[A-Za-z0-9_./-]+"
    r"|\u00a7"
    r"|\bb/"
    r"|\b(?:p|sec|ch|tbl|fig)\."
    r"|\b(?:page|section|chapter|clause|table|figure|doc(?:ument)?|trm|"
    r"datasheet|errata|hpg|rev(?:ision)?|evidence|run[-_]?id)\b"
    r"|\b[A-Za-z0-9_-]{25,}\b)",
    re.I,
)
_QUOTE_DELIMITER_RE = re.compile(
    r"(?:--|\u2014|\u2013)\s*[^\n]*?"
    r'(?:"[^"\n]+"|\u201c[^\u201d\n]+\u201d|`[^`\n]+`|\'[^\'\n]+\')'
)
_SYNTHETIC_RE = re.compile(
    r"(?i)\b(simulated|synthetic|mock(?:ed)?|fake|placeholder)\s+"
    r"(?:register\s+)?(value|values|read|reads|data|evidence|bundle)"
)
_SHA256_RE = re.compile(r"sha256:([0-9a-f]{8,64})", re.I)

WORKFLOW_GATES = {
    "fix": ["pre-fix", "pre-commit"],
    "recon": ["static", "verified"],
}


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


def check_clean_repo(repo_root: Path) -> Tuple[bool, str]:
    """Checks `git status --porcelain` under `repo_root` is empty."""
    if not repo_root.is_dir():
        return False, f"`{repo_root}` is not a directory"
    try:
        proc = subprocess.run(
            ["git", "-C", str(repo_root), "status", "--porcelain"],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )
        if proc.returncode != 0:
            err = (proc.stderr or proc.stdout or "").strip()
            return (
                False,
                f"`git status --porcelain` failed ({proc.returncode}): {err}",
            )
        dirty = [
            line.strip()
            for line in (proc.stdout or "").splitlines()
            if line.strip()
        ]
        if dirty:
            preview = ", ".join(dirty[:5])
            if len(dirty) > 5:
                preview += f", ... (+{len(dirty) - 5} more)"
            return False, preview
        return True, ""
    except (OSError, subprocess.SubprocessError) as exc:
        return False, f"Unable to run `git status --porcelain`: {exc}"


def _read_text(path: Path) -> str:
    return (
        path.read_text(encoding="utf-8", errors="replace")
        if path.is_file()
        else ""
    )


# ---------------------------------------------------------------------------
# Fix workflow (`/autoda-fix`)
# ---------------------------------------------------------------------------


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
        devlog_text = _read_text(bug_devlog_path)
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

    spec_text = _read_text(bug_spec_path)
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


# ---------------------------------------------------------------------------
# Recon workflow (`/autoda-recon`)
# ---------------------------------------------------------------------------


def _claim_field(block: str, field: str) -> str:
    m = re.search(_CLAIM_FIELD_RE.format(field=re.escape(field)), block)
    return m.group(1).strip() if m else ""


def parse_claims(spec_text: str) -> List[Dict[str, str]]:
    """Parses `- **C<n>** ...` claim blocks under `## 3. Claims`."""
    body = extract_section_body(spec_text, _RECON_CLAIMS_HEADING)
    if not body:
        return []
    headers = list(_CLAIM_HEADER_RE.finditer(body))
    claims: List[Dict[str, str]] = []
    for i, m in enumerate(headers):
        end = headers[i + 1].start() if i + 1 < len(headers) else len(body)
        block = body[m.start() : end]
        header_line = m.group(2)
        markers = re.findall(r"\[([A-Z_]+)\]", header_line)
        tiers = [t for t in markers if t in _RECON_TIERS]
        statuses = [s for s in markers if s in _RECON_STATUSES]
        claims.append(
            {
                "id": f"C{m.group(1)}",
                "tier": tiers[0] if tiers else "",
                "status": statuses[0] if statuses else "",
                "applies_to": _claim_field(block, "Applies-to"),
                "source": _claim_field(block, "Source"),
                "evidence": _claim_field(block, "Evidence"),
                "block": block,
            }
        )
    return claims


def _audit_recon_claim_counts(
    fm: Dict[str, str],
    claims: List[Dict[str, str]],
    artifact_label: str,
    require_verified_count: bool = False,
) -> List[str]:
    """Cross-checks frontmatter claim count fields against parsed claims."""
    violations: List[str] = []
    actual_counts = {
        "claims_total": len(claims),
        "claims_contradicted": sum(
            1 for c in claims if c["status"] == "CONTRADICTED"
        ),
        "claims_verified": sum(1 for c in claims if c["status"] == "VERIFIED"),
    }
    required_keys = {"claims_total", "claims_contradicted"}
    if require_verified_count or actual_counts["claims_verified"] > 0:
        required_keys.add("claims_verified")

    for key, actual in actual_counts.items():
        if key not in fm and key not in required_keys:
            continue
        raw = fm.get(key, "").strip()
        if not raw or not re.fullmatch(r"\d+", raw):
            violations.append(
                f"[STATIC_INVALID_CLAIM_COUNT] `{artifact_label}` frontmatter "
                f"`{key}` must be a non-negative integer matching parsed "
                f"claims ({actual}), got: {raw!r}."
            )
        elif int(raw) != actual:
            violations.append(
                f"[STATIC_CLAIM_COUNT_MISMATCH] `{artifact_label}` frontmatter "
                f"`{key}` is {int(raw)}, but `## 3. Claims` contains {actual} "
                "matching claim(s)."
            )
    return violations


def audit_recon_static(
    spec_path: Path,
    devlog_path: Path,
    repo_root: Optional[Path] = None,
) -> List[str]:
    """Runs the recon `static` gate (end of static analysis)."""
    violations: List[str] = []

    if not spec_path.is_file():
        violations.append(
            f"[STATIC_MISSING_RECON_SPEC] `{spec_path}` does not exist."
        )
        return violations

    spec_text = spec_path.read_text(encoding="utf-8", errors="replace")
    devlog_text = _read_text(devlog_path)
    fm = parse_frontmatter(spec_text)
    devlog_fm: Dict[str, str] = {}
    if not fm:
        violations.append(
            "[STATIC_MISSING_FRONTMATTER] `recon_spec.md` is missing YAML "
            "frontmatter (`--- ... ---`)."
        )
    else:
        if not fm.get("system_id", "").strip():
            violations.append(
                "[STATIC_MISSING_SYSTEM_ID] `recon_spec.md` frontmatter "
                "`system_id` is empty."
            )
        if not fm.get("target_scope", "").strip():
            violations.append(
                "[STATIC_MISSING_TARGET_SCOPE] `recon_spec.md` frontmatter "
                "`target_scope` is empty; record the SoC, IP revision, "
                "silicon revision, and board the claims apply to."
            )
        hw = fm.get("hardware_verification", "")
        if hw not in _RECON_HW_VERIFICATION:
            violations.append(
                "[STATIC_INVALID_HW_VERIFICATION] `recon_spec.md` frontmatter "
                "`hardware_verification` must be 'enabled' or 'disabled', "
                f"got: {hw!r}."
            )
        outcome = fm.get("outcome", "")
        if outcome not in _RECON_OUTCOMES:
            violations.append(
                "[STATIC_INVALID_OUTCOME] `recon_spec.md` frontmatter "
                f"`outcome` must be one of {sorted(_RECON_OUTCOMES)}, "
                f"got: {outcome!r}."
            )

    if not devlog_path.is_file():
        violations.append(
            f"[STATIC_MISSING_RECON_DEVLOG] `{devlog_path}` does not exist."
        )
    else:
        devlog_fm = parse_frontmatter(devlog_text)
        if not devlog_fm:
            violations.append(
                "[STATIC_MISSING_DEVLOG_FRONTMATTER] `recon_devlog.md` is "
                "missing YAML frontmatter (`--- ... ---`)."
            )
        elif fm and devlog_fm.get("system_id", "") != fm.get("system_id", ""):
            violations.append(
                "[STATIC_SYSTEM_ID_DRIFT] `system_id` in `recon_devlog.md` "
                f"({devlog_fm.get('system_id', '')!r}) does not match "
                f"`recon_spec.md` ({fm.get('system_id', '')!r})."
            )

    for label, pattern in _REQUIRED_RECON_SECTIONS:
        body = extract_section_body(spec_text, pattern)
        if not re.search(pattern, spec_text):
            violations.append(
                f"[STATIC_MISSING_SECTION] `recon_spec.md` is missing "
                f"required heading `{label}`."
            )
        elif len(body) < 20:
            violations.append(
                f"[STATIC_EMPTY_SECTION] `recon_spec.md` section `{label}` "
                "is empty or placeholder-only."
            )

    claims = parse_claims(spec_text)
    if re.search(_RECON_CLAIMS_HEADING, spec_text) and not claims:
        violations.append(
            "[STATIC_NO_CLAIMS] `## 3. Claims` contains no `- **C<n>**` "
            "claim blocks."
        )
    if fm:
        violations.extend(
            _audit_recon_claim_counts(fm, claims, "recon_spec.md")
        )
    if (
        devlog_fm
        and fm.get("outcome", "") in ("STATIC_COMPLETE", "HARDWARE_VERIFIED")
        and any(
            k in devlog_fm
            for k in ("claims_total", "claims_contradicted", "claims_verified")
        )
    ):
        violations.extend(
            _audit_recon_claim_counts(devlog_fm, claims, "recon_devlog.md")
        )

    for claim in claims:
        cid = claim["id"]
        if not claim["tier"]:
            violations.append(
                f"[STATIC_CLAIM_MISSING_TIER] {cid} has no confidence tier "
                f"marker; expected one of {sorted(_RECON_TIERS)}."
            )
        if not claim["status"]:
            violations.append(
                f"[STATIC_CLAIM_MISSING_STATUS] {cid} has no status marker; "
                f"expected one of {sorted(_RECON_STATUSES)}."
            )
        if _PLACEHOLDER_SCOPE_RE.match(claim["applies_to"] or ""):
            violations.append(
                f"[STATIC_CLAIM_MISSING_SCOPE] {cid} has no concrete "
                "`Applies-to:` scope (SoC, IP block and revision, silicon "
                "revision). Placeholder or unscoped tokens are forbidden."
            )
        source = claim["source"]
        unquoted_source = _QUOTED_SPAN_RE.sub("", source)
        if len(source) < 10 or not _SOURCE_ANCHOR_RE.search(unquoted_source):
            violations.append(
                f"[STATIC_CLAIM_MISSING_SOURCE] {cid} has no `Source:` anchor "
                "(path@commit:lines, document/URI locator, section/page, bug "
                "comment, or URL)."
            )
        elif not _QUOTE_DELIMITER_RE.search(source):
            violations.append(
                f"[STATIC_CLAIM_MISSING_QUOTE] {cid} `Source:` has an anchor "
                'but lacks a verbatim quote after `-- "..."`.'
            )
        if claim["status"] == "VERIFIED":
            evidence = claim["evidence"]
            if claim["tier"] != "HW_VERIFIED":
                violations.append(
                    f"[STATIC_CLAIM_VERIFIED_WITHOUT_HW] {cid} is `[VERIFIED]` "
                    f"with tier `[{claim['tier']}]`; only `[HW_VERIFIED]` "
                    "claims may be verified. Documentation or source reading "
                    "alone never verifies a claim."
                )
            if not (
                _SHA256_RE.search(evidence)
                and re.search(r"(?i)boot_id", evidence)
            ):
                violations.append(
                    f"[STATIC_CLAIM_MISSING_EVIDENCE] {cid} is `[VERIFIED]` "
                    "but its `Evidence:` line does not cite a `sha256:` "
                    "manifest hash and a `boot_id`."
                )
        elif claim["tier"] == "HW_VERIFIED":
            violations.append(
                f"[STATIC_CLAIM_TIER_STATUS_MISMATCH] {cid} has tier "
                f"`[HW_VERIFIED]` but status `[{claim['status']}]`."
            )

    combined = spec_text + "\n" + devlog_text
    if not re.search(
        r"VERIFIED_FROM_SOURCE|BLOCKED_MISSING_LINUX_SOURCE", combined
    ):
        violations.append(
            "[STATIC_MISSING_LINUX_STATUS] Neither artifact records the "
            "`linux-driver-expert` status marker (`VERIFIED_FROM_SOURCE` or "
            "`BLOCKED_MISSING_LINUX_SOURCE`)."
        )
    if not re.search(
        r"VERIFIED_FROM_DATASHEET|UNAVAILABLE_NO_DATASHEET_FOUND", combined
    ):
        violations.append(
            "[STATIC_MISSING_DATASHEET_STATUS] Neither artifact records the "
            "`datasheet-researcher` status marker (`VERIFIED_FROM_DATASHEET` "
            "or `UNAVAILABLE_NO_DATASHEET_FOUND`)."
        )

    for label, heading in (
        ("## 3. Claims", _RECON_CLAIMS_HEADING),
        (
            "## 7. Hardware Verification Plan & Evidence",
            _RECON_EVIDENCE_HEADING,
        ),
    ):
        body = extract_section_body(spec_text, heading)
        m = _SYNTHETIC_RE.search(body)
        if m:
            violations.append(
                f"[STATIC_SYNTHETIC_DATA] `{label}` mentions "
                f"{m.group(0)!r}; simulated or synthetic values must never "
                "enter a recon spec."
            )

    if repo_root is not None:
        clean, detail = check_clean_repo(repo_root)
        if not clean:
            violations.append(
                f"[STATIC_REPO_DIRTY] Repository `{repo_root}` is not clean "
                f"after static recon ({detail}). Read-only roles must never "
                "modify checkout files."
            )

    return violations


def audit_recon_verified(
    spec_path: Path,
    devlog_path: Path,
    skip_live_target_check: bool = False,
    repo_root: Optional[Path] = None,
) -> List[str]:
    """Runs the recon `verified` gate (after hardware verification)."""
    violations = audit_recon_static(spec_path, devlog_path)
    if not spec_path.is_file() or not devlog_path.is_file():
        return violations

    spec_text = spec_path.read_text(encoding="utf-8", errors="replace")
    devlog_text = devlog_path.read_text(encoding="utf-8", errors="replace")
    fm = parse_frontmatter(spec_text)

    if fm.get("hardware_verification", "") != "enabled":
        violations.append(
            "[VERIFIED_HW_DISABLED] The `verified` gate requires "
            '`hardware_verification: "enabled"` in `recon_spec.md`.'
        )

    claims = parse_claims(spec_text)
    verified = [c for c in claims if c["status"] == "VERIFIED"]
    if not verified:
        violations.append(
            "[VERIFIED_NO_VERIFIED_CLAIMS] No claim is `[VERIFIED]`; the "
            "`verified` gate requires at least one hardware-verified claim."
        )

    for marker, label in (
        (r"manifest\.json", "`manifest.json`"),
        (r"target-audit\.jsonl", "`target-audit.jsonl`"),
        (r"boot_id", "`boot_id`"),
    ):
        if not re.search(marker, devlog_text):
            violations.append(
                f"[VERIFIED_MISSING_DEVLOG_EVIDENCE] `recon_devlog.md` never "
                f"mentions {label}; driver-lab evidence must be logged."
            )

    for claim in verified:
        for digest in _SHA256_RE.findall(claim["evidence"]):
            if digest.lower() not in devlog_text.lower():
                violations.append(
                    f"[VERIFIED_EVIDENCE_NOT_IN_DEVLOG] {claim['id']} cites "
                    f"manifest `sha256:{digest}` that does not appear in "
                    "`recon_devlog.md`."
                )

    if not skip_live_target_check:
        has_devlog_hw = bool(
            re.search(
                r"(?i)(ffx\s+target\s+list|driver-lab|RCS\s*:\s*Y|Target\s+attached)",
                devlog_text,
            )
        )
        ok, reason = check_live_target()
        if not ok and not has_devlog_hw:
            violations.append(
                "[VERIFIED_MISSING_HARDWARE_TARGET] Hardware verification "
                f"requires an attached target, but {reason} and "
                "`recon_devlog.md` has no target attachment record."
            )

    if repo_root is not None:
        clean, detail = check_clean_repo(repo_root)
        if not clean and not re.search(r"(?i)\benablement\b", devlog_text):
            violations.append(
                f"[VERIFIED_REPO_DIRTY] Repository `{repo_root}` has "
                f"unlogged modifications ({detail}); any WIP node-enablement "
                "patch must be documented in `recon_devlog.md`."
            )

    return violations


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def run_gate(
    workflow: str,
    gate: str,
    spec_path: Path,
    devlog_path: Path,
    skip_live_target_check: bool = False,
    repo_root: Optional[Path] = None,
) -> List[str]:
    """Dispatches to the audit function for `workflow` / `gate`."""
    if gate not in WORKFLOW_GATES.get(workflow, []):
        raise ValueError(
            f"Gate {gate!r} is not valid for workflow {workflow!r}; "
            f"expected one of {WORKFLOW_GATES.get(workflow, [])}."
        )
    if workflow == "fix":
        if gate == "pre-fix":
            return audit_pre_fix(spec_path, devlog_path, skip_live_target_check)
        return audit_pre_commit(spec_path, devlog_path, skip_live_target_check)
    if gate == "static":
        return audit_recon_static(spec_path, devlog_path, repo_root=repo_root)
    return audit_recon_verified(
        spec_path,
        devlog_path,
        skip_live_target_check=skip_live_target_check,
        repo_root=repo_root,
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Deterministic compliance auditor for AutoDA sessions "
            "(autoda-fix and autoda-recon)."
        )
    )
    parser.add_argument(
        "--workflow",
        default="fix",
        choices=sorted(WORKFLOW_GATES),
        help="Entry-point workflow being audited (default: fix).",
    )
    parser.add_argument(
        "--gate",
        required=True,
        choices=sorted({g for gates in WORKFLOW_GATES.values() for g in gates}),
        help=(
            "Audit gate to enforce. Fix: 'pre-fix' (Gate 1) or 'pre-commit' "
            "(Gate 2). Recon: 'static' or 'verified'."
        ),
    )
    parser.add_argument(
        "--spec",
        "--bug-spec",
        dest="spec",
        required=True,
        help="Path to the spec artifact (bug_spec.md or recon_spec.md).",
    )
    parser.add_argument(
        "--devlog",
        "--bug-devlog",
        dest="devlog",
        required=True,
        help="Path to the devlog artifact (bug_devlog.md or recon_devlog.md).",
    )
    parser.add_argument(
        "--skip-live-target-check",
        action="store_true",
        help="Skip live `ffx target list` subprocess check (rely on artifact text).",
    )
    parser.add_argument(
        "--repo-root",
        default=None,
        help=(
            "Optional repository root to verify clean `git status --porcelain` "
            "during recon gates."
        ),
    )
    args = parser.parse_args()

    try:
        violations = run_gate(
            args.workflow,
            args.gate,
            Path(args.spec),
            Path(args.devlog),
            skip_live_target_check=args.skip_live_target_check,
            repo_root=Path(args.repo_root) if args.repo_root else None,
        )
    except ValueError as exc:
        parser.error(str(exc))

    result = {
        "workflow": args.workflow,
        "gate": args.gate,
        "status": "PASS" if not violations else "FAIL",
        "violation_count": len(violations),
        "violations": violations,
    }
    print(json.dumps(result, indent=2))
    return 0 if not violations else 1


if __name__ == "__main__":
    sys.exit(main())
