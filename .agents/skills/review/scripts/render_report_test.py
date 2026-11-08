#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for render_report.py.

Run: python3 -B .agents/skills/review/scripts/render_report_test.py
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import render_report  # pylint: disable=wrong-import-position


class RenderReportTest(unittest.TestCase):
    def test_full_report(self):
        payload = {
            "decision": "CHANGES_REQUESTED",
            "summary": "Found one blocking issue.",
            "scope": "Working copy",
            "agent_skills": ["rust_best_practices", "general"],
            "findings": [
                {
                    "severity": "suggestion",
                    "comment": "Document panics.",
                    "rule_reference": "rust_best_practices/docs",
                    "rule_link": "file:///repo/.agents/skills/r/SKILL.md#L1-L2",
                    "file_path": "src/lib.rs",
                    "line_start": 46,
                },
                {
                    "severity": "blocking",
                    "comment": "Missing tests.",
                    "rule_reference": "general/tests",
                },
                {
                    "severity": "blocking",
                    "comment": "unwrap() on input.",
                    "file_path": "src/lib.rs",
                    "line_start": 47,
                    "line_end": 48,
                    "code_snippet": "fn f() {\n  x.unwrap()\n}",
                    "suggested_diff": "-a\n+b",
                },
            ],
        }
        expected = """\
## Code Review

❌ **Changes requested** — See inline comments for details.

Found one blocking issue.

- **Scope**: Working copy
- **Skills Evaluated**: `rust_best_practices`, `general`

---

- **#1**

  **🚨 Blocking** (*general/tests*): Missing tests.

---

### [`src/lib.rs`](file:///repo/src/lib.rs)

- **#2** [**L46**](file:///repo/src/lib.rs#L46)

  **💡 Suggestion** ([**rust_best_practices/docs**](file:///repo/.agents/skills/r/SKILL.md#L1-L2)): Document panics.

- **#3** [**L47-48** `fn f() { ...`](file:///repo/src/lib.rs#L47-L48)

  **🚨 Blocking**: unwrap() on input.

  ```diff
  -a
  +b
  ```
"""
        self.assertEqual(render_report.render(payload, "/repo"), expected)
        self.assertEqual(
            [f["finding_id"] for f in payload["findings"]], ["1", "2", "3"]
        )

    def test_approved_without_findings(self):
        self.assertEqual(
            render_report.render({"decision": "approved"}),
            "## Code Review\n\n✅ **Approved** — All checks pass.\n",
        )

    def test_backticks_in_snippet_and_diff(self):
        payload = {
            "decision": "approved_with_suggestions",
            "findings": [
                {
                    "severity": "suggestion",
                    "comment": "Fix the example.",
                    "file_path": "docs/a.md",
                    "line_start": 3,
                    "code_snippet": "Run `fx build`.",
                    "suggested_diff": "-```\n+```sh\n fx build\n ```",
                },
            ],
        }
        expected = """\
- **#1** [**L3** `` Run `fx build`. ``](file:///repo/docs/a.md#L3)

  **💡 Suggestion**: Fix the example.

  ````diff
  -```
  +```sh
   fx build
   ```
  ````
"""
        self.assertTrue(
            render_report.render(payload, "/repo").endswith(expected)
        )

    def test_invalid_decision(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render({"decision": "lgtm"})

    def test_invalid_severity(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render(
                {
                    "decision": "approved",
                    "findings": [{"severity": "nit", "comment": "x"}],
                }
            )

    def test_empty_comment(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render(
                {
                    "decision": "approved",
                    "findings": [{"severity": "blocking", "comment": " "}],
                }
            )

    def test_bad_line_range(self):
        with self.assertRaises(render_report.ReportError):
            render_report.render(
                {
                    "decision": "approved",
                    "findings": [
                        {
                            "severity": "blocking",
                            "comment": "x",
                            "file_path": "a",
                            "line_start": 5,
                            "line_end": 4,
                        }
                    ],
                }
            )

    def test_confidence_and_skipped_files_rendering(self):
        payload = {
            "decision": "changes_requested",
            "scope": "Working copy",
            "skipped_files": ["Cargo.lock", "docs/a.png"],
            "findings": [
                {
                    "severity": "blocking",
                    "confidence": 95,
                    "comment": "Panics on empty slice.",
                    "rule_reference": "rust_best_practices/panics",
                    "file_path": "src/lib.rs",
                    "line_start": 10,
                }
            ],
        }
        rendered = render_report.render(payload, "/repo")
        self.assertIn(
            "- **Skipped Files**: 2 files matched skip globs (`Cargo.lock`,"
            " `docs/a.png`)",
            rendered,
        )
        self.assertIn(
            "**🚨 Blocking** (Confidence: 95%)"
            " (*rust_best_practices/panics*): Panics on empty slice.",
            rendered,
        )

    def test_min_confidence_filters_and_recomputes_decision(self):
        payload = {
            "decision": "changes_requested",
            "findings": [
                {
                    "severity": "blocking",
                    "confidence": 65,
                    "comment": "Speculative concern.",
                    "file_path": "src/lib.rs",
                    "line_start": 5,
                },
                {
                    "severity": "suggestion",
                    "confidence": 85,
                    "comment": "Add doc comment.",
                    "file_path": "src/lib.rs",
                    "line_start": 12,
                },
            ],
        }
        rendered = render_report.render(payload, "/repo", min_confidence=80)
        self.assertIn("**Approved with minor suggestions**", rendered)
        self.assertNotIn("Speculative concern.", rendered)
        self.assertIn("Add doc comment.", rendered)
        self.assertEqual(len(payload["findings"]), 1)

    def test_invalid_confidence_rejected(self):
        for bad in (-1, 101, True, "90"):
            with self.subTest(bad=bad):
                with self.assertRaises(render_report.ReportError):
                    render_report.render(
                        {
                            "decision": "changes_requested",
                            "findings": [
                                {
                                    "severity": "blocking",
                                    "confidence": bad,
                                    "comment": "x",
                                }
                            ],
                        }
                    )

    def test_pre_existing_tags_and_downgrades_to_suggestion(self):
        payload = {
            "decision": "changes_requested",
            "scope": "Working copy",
            "findings": [
                {
                    "severity": "blocking",
                    "pre_existing": True,
                    "confidence": 88,
                    "comment": "Unchecked error in moved helper.",
                    "rule_reference": "general/reliability",
                    "file_path": "src/lib.rs",
                    "line_start": 10,
                },
                {
                    "severity": "suggestion",
                    "pre_existing": True,
                    "confidence": 85,
                    "comment": "[Pre-existing]: Legacy race on shutdown.",
                    "file_path": "src/lib.rs",
                    "line_start": 25,
                },
            ],
        }
        rendered = render_report.render(payload, "/repo")
        self.assertIn("**Approved with minor suggestions**", rendered)
        self.assertIn(
            "**💡 Suggestion** (Confidence: 88%)"
            " (*general/reliability*): [Pre-existing] Unchecked error in"
            " moved helper.",
            rendered,
        )
        self.assertIn(
            "**💡 Suggestion** (Confidence: 85%): [Pre-existing] Legacy race"
            " on shutdown.",
            rendered,
        )
        self.assertTrue(
            all(f["severity"] == "suggestion" for f in payload["findings"])
        )

    def test_invalid_pre_existing_rejected(self):
        for bad in ("true", 1):
            with self.subTest(bad=bad):
                with self.assertRaises(render_report.ReportError):
                    render_report.render(
                        {
                            "decision": "approved_with_suggestions",
                            "findings": [
                                {
                                    "severity": "suggestion",
                                    "pre_existing": bad,
                                    "comment": "x",
                                }
                            ],
                        }
                    )

    def test_pre_existing_prefix_only_comment_rejected(self):
        for comment in ("[Pre-existing]   ", "[Pre-existing][Preexisting] : "):
            with self.subTest(comment=comment):
                with self.assertRaises(render_report.ReportError):
                    render_report.render(
                        {
                            "decision": "approved_with_suggestions",
                            "findings": [
                                {
                                    "severity": "suggestion",
                                    "pre_existing": True,
                                    "comment": comment,
                                }
                            ],
                        }
                    )

    def test_non_list_findings_raises_report_error(self):
        for bad in (5, True, "bad", {"a": 1}):
            with self.subTest(bad=bad):
                with self.assertRaises(render_report.ReportError):
                    render_report.render(
                        {"decision": "approved", "findings": bad}
                    )

    def test_pre_existing_edge_cases_and_decision_recomputation(self):
        # Stale changes_requested with only a pre_existing suggestion becomes
        # approved_with_suggestions.
        stale = {
            "decision": "changes_requested",
            "findings": [
                {
                    "severity": "suggestion",
                    "pre_existing": True,
                    "comment": "[Pre-existing] [Preexisting]: Old helper bug.",
                }
            ],
        }
        rendered = render_report.render(stale, "/repo")
        self.assertIn("**Approved with minor suggestions**", rendered)
        self.assertIn("[Pre-existing] Old helper bug.", rendered)

        # Mixed pre_existing blocking + genuine blocking stays changes_requested.
        mixed = {
            "decision": "changes_requested",
            "findings": [
                {
                    "severity": "blocking",
                    "pre_existing": True,
                    "confidence": 88,
                    "comment": "Moved bug.",
                },
                {
                    "severity": "blocking",
                    "pre_existing": False,
                    "confidence": 92,
                    "comment": "[Pre-existing] New bug with literal prefix.",
                },
            ],
        }
        rendered_mixed = render_report.render(mixed, "/repo", min_confidence=80)
        self.assertIn("**Changes requested**", rendered_mixed)
        self.assertEqual(mixed["findings"][0]["severity"], "suggestion")
        self.assertEqual(mixed["findings"][1]["severity"], "blocking")

        # Filtered-out pre_existing finding leaves approved.
        filtered = {
            "decision": "changes_requested",
            "findings": [
                {
                    "severity": "blocking",
                    "pre_existing": True,
                    "confidence": 70,
                    "comment": "Low-confidence legacy bug.",
                }
            ],
        }
        self.assertIn(
            "**Approved** — All checks pass.",
            render_report.render(filtered, "/repo", min_confidence=80),
        )

        # not_applicable stays not_applicable.
        na = {"decision": "not_applicable", "findings": []}
        self.assertIn("**N/A**", render_report.render(na, "/repo"))

    def test_quick_mode_contract_enforcement(self):
        payload = {
            "decision": "changes_requested",
            "effort": "quick",
            "findings": [
                {
                    "severity": "suggestion",
                    "confidence": 95,
                    "comment": "Style suggestion.",
                },
                {
                    "severity": "blocking",
                    "pre_existing": True,
                    "confidence": 95,
                    "comment": "Moved legacy bug.",
                },
                {
                    "severity": "blocking",
                    "comment": "Missing confidence score.",
                },
                {
                    "severity": "blocking",
                    "confidence": 92,
                    "comment": "Real panic on empty slice.",
                },
            ],
        }
        rendered = render_report.render(payload, "/repo", min_confidence=90)
        self.assertIn("- **Effort**: Quick", rendered)
        self.assertIn("**Changes requested**", rendered)
        self.assertEqual(len(payload["findings"]), 1)
        self.assertEqual(
            payload["findings"][0]["comment"], "Real panic on empty slice."
        )

    def test_effort_header_and_cli_override(self):
        payload = {
            "decision": "approved",
            "scope": "fuchsia.git @ 1a2b3c4d",
            "effort": "DEEP",
        }
        rendered = render_report.render(payload, "/repo")
        self.assertIn("- **Effort**: Deep", rendered)
        self.assertEqual(payload["effort"], "deep")

        empty_effort = {"decision": "approved", "effort": "   "}
        rendered_empty = render_report.render(empty_effort, "/repo")
        self.assertNotIn("- **Effort**:", rendered_empty)
        self.assertNotIn("effort", empty_effort)

        for bad in ("turbo", 1, True):
            with self.subTest(bad=bad):
                with self.assertRaises(render_report.ReportError):
                    render_report.render(
                        {"decision": "approved", "effort": bad}
                    )

        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "review.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(
                    {
                        "decision": "changes_requested",
                        "scope": "Working copy",
                        "findings": [
                            {
                                "severity": "blocking",
                                "pre_existing": True,
                                "confidence": 85,
                                "comment": "Moved bug.",
                            }
                        ],
                    },
                    f,
                )
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = render_report.main(
                    [
                        f"--review_result_file={path}",
                        "--effort=STANDARD",
                        "--min_confidence=80",
                    ]
                )
            self.assertEqual(rc, 0)
            self.assertIn("- **Effort**: Standard", out.getvalue())
            self.assertIn("**Approved with minor suggestions**", out.getvalue())
            with open(path, encoding="utf-8") as f:
                saved = json.load(f)
            self.assertEqual(saved["effort"], "standard")
            self.assertEqual(saved["decision"], "approved_with_suggestions")
            self.assertEqual(saved["findings"][0]["severity"], "suggestion")

            # Invalid JSON effort is still caught even when --effort is passed.
            bad_path = os.path.join(tmp, "bad.json")
            with open(bad_path, "w", encoding="utf-8") as f:
                json.dump({"decision": "approved", "effort": "turbo"}, f)
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(
                    render_report.main(
                        [
                            f"--review_result_file={bad_path}",
                            "--effort=deep",
                        ]
                    ),
                    1,
                )


if __name__ == "__main__":
    unittest.main()
