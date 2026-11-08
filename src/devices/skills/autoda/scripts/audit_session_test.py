#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for audit_session.py (fix and recon workflows).

Run with:
    python3 src/devices/skills/autoda/scripts/audit_session_test.py
"""

import pathlib
import subprocess
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import audit_session  # pylint: disable=wrong-import-position

_FIX_SPEC = textwrap.dedent(
    """\
    ---
    skill: autoda-fix
    skill_version: "1.3.0"
    verification_mode: "{mode}"
    adversarial_review: "disabled"
    ---
    # Bug spec

    ## 1. Symptoms & Evidence
    The driver logs a timeout on every resume cycle after suspend.

    ## 2. Candidate Hypotheses & Discriminating Experiments
    - **H1** `[VERIFIED]` -- the completion path skips the callback.

    ## 3. Validated Root Cause
    - **H1** `[VERIFIED]` -- completion handler returns before Control().

    ## 3b. Cross-Subagent Contradiction & Reconciliation Ledger
    All subagent reports agreed on the callback ordering; no contradictions.

    ## 3c. In-Driver & End-to-End Invariant Trace
    Axis 1: sibling endpoints round lengths the same way. Axis 2: dataflow
    continuity traced to Control(). Axis 3: litmus test passed.
    """
)

_FIX_DEVLOG = textwrap.dedent(
    """\
    ---
    skill: autoda-fix
    verification_mode: "{mode}"
    adversarial_review: "disabled"
    ---
    ## Phase 0
    Reconnaissance complete. Target attached; `ffx target list` shows RCS: Y.
    Ran fx test driver_unittests: PASS.
    """
)

_RECON_SPEC = textwrap.dedent(
    """\
    ---
    skill: autoda-recon
    skill_version: "1.0.0"
    system_id: "{system_id}"
    target_scope: "example-soc / example-timer v2 / silicon B0 / dev-board"
    hardware_verification: "{hw}"
    outcome: "{outcome}"
    claims_total: {claims_total}
    claims_verified: {claims_verified}
    claims_contradicted: {claims_contradicted}
    ---
    # Recon spec

    ## 1. System Identity & Scope
    Example timer block, IP revision v2, on example-soc silicon B0.

    ## 2. Reference Sources & Provenance
    - Linux: vendor-kernel `drivers/clocksource/example-timer.c` @ abc1234
      (`STATUS: VERIFIED_FROM_SOURCE`).
    - Datasheet: `STATUS: UNAVAILABLE_NO_DATASHEET_FOUND`.

    ## 3. Claims
    {claims}

    ## 4. Dependencies & Bring-Up Order
    Requires `clk_timer` enabled and `rst_timer` deasserted before any access.

    ## 5. Cross-Subagent Contradiction & Reconciliation Ledger
    No contradictions between linux-driver-expert and datasheet-researcher.

    ## 6. Gaps & Blocked Sources
    No vendor TRM located for this block; searched the documentation cache.

    ## 7. Hardware Verification Plan & Evidence
    {evidence}
    """
)

_CLAIM_OK = textwrap.dedent(
    """\
    - **C1** `[LINUX_DERIVED]` `[UNVERIFIED]` -- CTL (0x00) bit 0 enables the counter.
      - **Type:** bitfield
      - **Applies-to:** example-soc / example-timer v2 / silicon B0
      - **Source:** vendor-kernel drivers/clocksource/example-timer.c@abc1234:120-131 -- "writel(CTL_EN, base + CTL)"
      - **Reasoning:** Linux sets CTL_EN as the last step of probe.
    """
)

_CLAIM_VERIFIED = textwrap.dedent(
    """\
    - **C2** `[HW_VERIFIED]` `[VERIFIED]` -- STA (0x04) reads 0x00000002 after boot.
      - **Type:** register
      - **Applies-to:** example-soc / example-timer v2 / silicon B0 / dev-board
      - **Source:** evidence/run-001/target-audit.jsonl seq 12 -- "read32 offset 4 value 2"
      - **Evidence:** manifest sha256:9f2a7c1e boot_id 01918a3b seq 12-13 state_origin firmware-configured
    """
)

_RECON_DEVLOG = textwrap.dedent(
    """\
    ---
    skill: autoda-recon
    system_id: "{system_id}"
    ---
    ## Phase 0
    Campaign config loaded. linux-driver-expert returned STATUS: VERIFIED_FROM_SOURCE.
    datasheet-researcher returned STATUS: UNAVAILABLE_NO_DATASHEET_FOUND.
    {hardware}
    """
)

_DEVLOG_HW = (
    "## Phase 3\n"
    "driver-lab run run-001: evidence/run-001/manifest.json sha256:9f2a7c1e, "
    "target-audit.jsonl seq 12-13, boot_id 01918a3b. `ffx target list` RCS: Y."
)


class _ArtifactCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write(self, name: str, text: str) -> pathlib.Path:
        path = self.root / name
        path.write_text(text, encoding="utf-8")
        return path

    @staticmethod
    def codes(violations):
        return {v.split("]", 1)[0].lstrip("[") for v in violations}


class FixWorkflowTest(_ArtifactCase):
    def test_mode_c_passes_both_gates(self) -> None:
        spec = self.write("bug_spec.md", _FIX_SPEC.format(mode="Mode C"))
        devlog = self.write("bug_devlog.md", _FIX_DEVLOG.format(mode="Mode C"))
        self.assertEqual(
            [], audit_session.run_gate("fix", "pre-fix", spec, devlog, True)
        )
        self.assertEqual(
            [], audit_session.run_gate("fix", "pre-commit", spec, devlog, True)
        )

    def test_hybrid_mode_label_is_rejected(self) -> None:
        spec = self.write(
            "bug_spec.md", _FIX_SPEC.format(mode="Mode A (Mode C fallback)")
        )
        devlog = self.write("bug_devlog.md", _FIX_DEVLOG.format(mode="Mode A"))
        codes = self.codes(
            audit_session.run_gate("fix", "pre-fix", spec, devlog, True)
        )
        self.assertIn("GATE1_INVALID_VERIFICATION_MODE", codes)

    def test_mode_drift_between_artifacts(self) -> None:
        spec = self.write("bug_spec.md", _FIX_SPEC.format(mode="Mode A"))
        devlog = self.write("bug_devlog.md", _FIX_DEVLOG.format(mode="Mode C"))
        codes = self.codes(
            audit_session.run_gate("fix", "pre-commit", spec, devlog, True)
        )
        self.assertIn("GATE2_MODE_DRIFT", codes)


class ReconWorkflowTest(_ArtifactCase):
    def _pair(
        self,
        claims,
        hw="disabled",
        evidence="Not yet run.",
        hardware="",
        outcome="IN_PROGRESS",
        claims_total=None,
        claims_verified=None,
        claims_contradicted=None,
    ):
        parsed = audit_session.parse_claims("## 3. Claims\n" + claims)
        if claims_total is None:
            claims_total = len(parsed)
        if claims_verified is None:
            claims_verified = sum(
                1 for c in parsed if c["status"] == "VERIFIED"
            )
        if claims_contradicted is None:
            claims_contradicted = sum(
                1 for c in parsed if c["status"] == "CONTRADICTED"
            )
        spec = self.write(
            "recon_spec.md",
            _RECON_SPEC.format(
                system_id="example-timer-0",
                hw=hw,
                outcome=outcome,
                claims_total=claims_total,
                claims_verified=claims_verified,
                claims_contradicted=claims_contradicted,
                claims=claims,
                evidence=evidence,
            ),
        )
        devlog = self.write(
            "recon_devlog.md",
            _RECON_DEVLOG.format(
                system_id="example-timer-0", hardware=hardware
            ),
        )
        return spec, devlog

    def test_static_gate_passes_on_scoped_cited_claim(self) -> None:
        spec, devlog = self._pair(_CLAIM_OK)
        self.assertEqual(
            [], audit_session.run_gate("recon", "static", spec, devlog, True)
        )

    def test_static_gate_rejects_unscoped_claim(self) -> None:
        unscoped = _CLAIM_OK.replace(
            "**Applies-to:** example-soc / example-timer v2 / silicon B0",
            "**Applies-to:** TBD",
        )
        spec, devlog = self._pair(unscoped)
        codes = self.codes(
            audit_session.run_gate("recon", "static", spec, devlog, True)
        )
        self.assertIn("STATIC_CLAIM_MISSING_SCOPE", codes)

    def test_static_gate_rejects_uncited_claim(self) -> None:
        uncited = _CLAIM_OK.replace(
            "**Source:** vendor-kernel drivers/clocksource/example-timer.c@abc1234:120-131"
            ' -- "writel(CTL_EN, base + CTL)"',
            "**Source:** the driver",
        )
        spec, devlog = self._pair(uncited)
        codes = self.codes(
            audit_session.run_gate("recon", "static", spec, devlog, True)
        )
        self.assertIn("STATIC_CLAIM_MISSING_SOURCE", codes)

    def test_verified_requires_hw_tier_and_evidence(self) -> None:
        doc_verified = _CLAIM_OK.replace("`[UNVERIFIED]`", "`[VERIFIED]`")
        spec, devlog = self._pair(doc_verified)
        codes = self.codes(
            audit_session.run_gate("recon", "static", spec, devlog, True)
        )
        self.assertIn("STATIC_CLAIM_VERIFIED_WITHOUT_HW", codes)
        self.assertIn("STATIC_CLAIM_MISSING_EVIDENCE", codes)

    def test_missing_status_markers_are_reported(self) -> None:
        spec, devlog = self._pair(_CLAIM_OK)
        text = spec.read_text(encoding="utf-8")
        spec.write_text(
            text.replace("VERIFIED_FROM_SOURCE", "n/a").replace(
                "UNAVAILABLE_NO_DATASHEET_FOUND", "n/a"
            ),
            encoding="utf-8",
        )
        devlog.write_text(
            _RECON_DEVLOG.format(system_id="example-timer-0", hardware="")
            .replace("VERIFIED_FROM_SOURCE", "n/a")
            .replace("UNAVAILABLE_NO_DATASHEET_FOUND", "n/a"),
            encoding="utf-8",
        )
        codes = self.codes(
            audit_session.run_gate("recon", "static", spec, devlog, True)
        )
        self.assertIn("STATIC_MISSING_LINUX_STATUS", codes)
        self.assertIn("STATIC_MISSING_DATASHEET_STATUS", codes)

    def test_synthetic_values_are_rejected(self) -> None:
        spec, devlog = self._pair(
            _CLAIM_OK, evidence="Populated with simulated register values."
        )
        codes = self.codes(
            audit_session.run_gate("recon", "static", spec, devlog, True)
        )
        self.assertIn("STATIC_SYNTHETIC_DATA", codes)

    def test_system_id_drift_is_reported(self) -> None:
        spec, devlog = self._pair(_CLAIM_OK)
        devlog.write_text(
            _RECON_DEVLOG.format(system_id="other-system", hardware=""),
            encoding="utf-8",
        )
        codes = self.codes(
            audit_session.run_gate("recon", "static", spec, devlog, True)
        )
        self.assertIn("STATIC_SYSTEM_ID_DRIFT", codes)

    def test_verified_gate_passes_with_devlog_evidence(self) -> None:
        spec, devlog = self._pair(
            _CLAIM_OK + _CLAIM_VERIFIED,
            hw="enabled",
            evidence="run-001 verified STA readback; see devlog.",
            hardware=_DEVLOG_HW,
        )
        self.assertEqual(
            [], audit_session.run_gate("recon", "verified", spec, devlog, True)
        )

    def test_verified_gate_rejects_evidence_missing_from_devlog(self) -> None:
        spec, devlog = self._pair(
            _CLAIM_OK + _CLAIM_VERIFIED,
            hw="enabled",
            evidence="run-001 verified STA readback.",
            hardware=_DEVLOG_HW.replace("sha256:9f2a7c1e", "sha256:deadbeef"),
        )
        codes = self.codes(
            audit_session.run_gate("recon", "verified", spec, devlog, True)
        )
        self.assertIn("VERIFIED_EVIDENCE_NOT_IN_DEVLOG", codes)

    def test_verified_gate_rejects_disabled_hardware(self) -> None:
        spec, devlog = self._pair(_CLAIM_OK, hw="disabled")
        codes = self.codes(
            audit_session.run_gate("recon", "verified", spec, devlog, True)
        )
        self.assertIn("VERIFIED_HW_DISABLED", codes)
        self.assertIn("VERIFIED_NO_VERIFIED_CLAIMS", codes)

    def test_claim_count_mismatch_and_invalid_count(self) -> None:
        spec, devlog = self._pair(_CLAIM_OK, claims_total=5)
        codes = self.codes(
            audit_session.run_gate("recon", "static", spec, devlog, True)
        )
        self.assertIn("STATIC_CLAIM_COUNT_MISMATCH", codes)

        contradicted = _CLAIM_OK.replace("`[UNVERIFIED]`", "`[CONTRADICTED]`")
        spec2, devlog2 = self._pair(contradicted, claims_contradicted=0)
        codes2 = self.codes(
            audit_session.run_gate("recon", "static", spec2, devlog2, True)
        )
        self.assertIn("STATIC_CLAIM_COUNT_MISMATCH", codes2)

        spec3, devlog3 = self._pair(_CLAIM_OK, claims_total="pending")
        codes3 = self.codes(
            audit_session.run_gate("recon", "static", spec3, devlog3, True)
        )
        self.assertIn("STATIC_INVALID_CLAIM_COUNT", codes3)

    def test_document_and_uri_anchors_require_verbatim_quote(self) -> None:
        doc_claim = textwrap.dedent(
            """\
            - **C1** `[DATASHEET]` `[UNVERIFIED]` -- CTL (0x00) resets to 0.
              - **Type:** register
              - **Applies-to:** example-soc / example-timer v2 / silicon B0
              - **Source:** doc:1lI_b0_abc1234567890abcdefg §4.2 -- "Reset value: 0x0000_0000"
              - **Reasoning:** Documented reset state.
            """
        )
        spec, devlog = self._pair(doc_claim)
        self.assertEqual(
            [], audit_session.run_gate("recon", "static", spec, devlog, True)
        )

        # Parenthetical title without `-- "..."` verbatim quote must fail.
        no_quote = doc_claim.replace(
            '§4.2 -- "Reset value: 0x0000_0000"',
            '("Timer Working Group Notes")',
        )
        spec_nq, devlog_nq = self._pair(no_quote)
        codes_nq = self.codes(
            audit_session.run_gate("recon", "static", spec_nq, devlog_nq, True)
        )
        self.assertIn("STATIC_CLAIM_MISSING_QUOTE", codes_nq)

        # Bare path@commit without verbatim quote must also fail.
        bare_path = _CLAIM_OK.replace(' -- "writel(CTL_EN, base + CTL)"', "")
        spec_bp, devlog_bp = self._pair(bare_path)
        codes_bp = self.codes(
            audit_session.run_gate("recon", "static", spec_bp, devlog_bp, True)
        )
        self.assertIn("STATIC_CLAIM_MISSING_QUOTE", codes_bp)

        # Slash inside quoted string must not masquerade as an anchor.
        fake_anchor = _CLAIM_OK.replace(
            "vendor-kernel drivers/clocksource/example-timer.c@abc1234:120-131"
            ' -- "writel(CTL_EN, base + CTL)"',
            'the driver -- "foo/bar writel(CTL_EN, base + CTL)"',
        )
        spec_fa, devlog_fa = self._pair(fake_anchor)
        codes_fa = self.codes(
            audit_session.run_gate("recon", "static", spec_fa, devlog_fa, True)
        )
        self.assertIn("STATIC_CLAIM_MISSING_SOURCE", codes_fa)

    def test_placeholder_scope_tokens_rejected(self) -> None:
        for bad_scope in (
            "SCOPE",
            "<scope>",
            "target_scope",
            "same",
            "same as above",
        ):
            bad_claim = _CLAIM_OK.replace(
                "example-soc / example-timer v2 / silicon B0", bad_scope
            )
            spec, devlog = self._pair(bad_claim)
            codes = self.codes(
                audit_session.run_gate("recon", "static", spec, devlog, True)
            )
            self.assertIn("STATIC_CLAIM_MISSING_SCOPE", codes, bad_scope)

    def test_repo_root_cleanliness_check(self) -> None:
        repo = self.root / "repo"
        repo.mkdir()
        subprocess.run(["git", "-C", str(repo), "init"], check=True)
        subprocess.run(
            [
                "git",
                "-C",
                str(repo),
                "config",
                "user.email",
                "test@example.com",
            ],
            check=True,
        )
        subprocess.run(
            ["git", "-C", str(repo), "config", "user.name", "Test"],
            check=True,
        )
        tracked = repo / "driver.c"
        tracked.write_text("int ok = 1;\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(repo), "add", "driver.c"], check=True)
        subprocess.run(
            ["git", "-C", str(repo), "commit", "-m", "init"], check=True
        )

        spec, devlog = self._pair(_CLAIM_OK)
        self.assertEqual(
            [],
            audit_session.run_gate(
                "recon", "static", spec, devlog, True, repo_root=repo
            ),
        )

        # Stray file created by a read-only subagent triggers STATIC_REPO_DIRTY.
        (repo / "stray.txt").write_text("oops\n", encoding="utf-8")
        codes = self.codes(
            audit_session.run_gate(
                "recon", "static", spec, devlog, True, repo_root=repo
            )
        )
        self.assertIn("STATIC_REPO_DIRTY", codes)

        # Verified gate rejects unlogged dirty state, but allows logged WIP enablement.
        v_spec, v_devlog = self._pair(
            _CLAIM_OK + _CLAIM_VERIFIED,
            hw="enabled",
            evidence="run-001 verified STA readback.",
            hardware=_DEVLOG_HW,
        )
        v_codes = self.codes(
            audit_session.run_gate(
                "recon", "verified", v_spec, v_devlog, True, repo_root=repo
            )
        )
        self.assertIn("VERIFIED_REPO_DIRTY", v_codes)

        v_spec2, v_devlog2 = self._pair(
            _CLAIM_OK + _CLAIM_VERIFIED,
            hw="enabled",
            evidence="run-001 verified STA readback.",
            hardware=_DEVLOG_HW + "\nWIP devicetree enablement patch applied.",
        )
        self.assertEqual(
            [],
            audit_session.run_gate(
                "recon", "verified", v_spec2, v_devlog2, True, repo_root=repo
            ),
        )


class DispatchTest(unittest.TestCase):
    def test_gate_must_match_workflow(self) -> None:
        with self.assertRaises(ValueError):
            audit_session.run_gate(
                "recon", "pre-fix", pathlib.Path("x"), pathlib.Path("y"), True
            )
        with self.assertRaises(ValueError):
            audit_session.run_gate(
                "fix", "static", pathlib.Path("x"), pathlib.Path("y"), True
            )


if __name__ == "__main__":
    unittest.main()
