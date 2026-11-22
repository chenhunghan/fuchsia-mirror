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
    def test_full_legacy_report(self):
        payload = {
            "decision": "CHANGES_REQUESTED",
            "summary": "Found two blocking issues and one suggestion.",
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
## Code review

❌ **Changes requested** (2 blocking, 1 suggestion)

Found two blocking issues and one suggestion.

- **Scope**: Working copy
- **Skills evaluated**: rust_best_practices, general

---

### Blocking issues

1. **🚨 Blocking** (*general/tests*)

   Missing tests.

2. [src/lib.rs:47-48](file:///repo/src/lib.rs#L47-L48)

   **🚨 Blocking**

   unwrap() on input.

   ```diff
   -a
   +b
   ```

---

### Suggestions

3. [src/lib.rs:46](file:///repo/src/lib.rs#L46)

   **💡 Suggestion** ([**rust_best_practices/docs**](file:///repo/.agents/skills/r/SKILL.md#L1-L2))

   Document panics.
"""
        self.assertEqual(render_report.render(payload, "/repo"), expected)
        self.assertEqual(
            [f["finding_id"] for f in payload["findings"]], ["1", "2", "3"]
        )

    def test_approved_without_findings(self):
        self.assertEqual(
            render_report.render({"decision": "approved"}),
            "## Code review\n\n🟢 **Approved**\n",
        )

    def test_structured_findings_and_trace_rendering(self):
        payload = {
            "decision": "changes_requested",
            "summary": "Reviewed library changes.",
            "scope": "Working copy",
            "effort": "standard",
            "challenger_kept": 2,
            "challenger_total": 4,
            "agent_skills": ["general"],
            "findings": [
                {
                    "severity": "suggestion",
                    "confidence": 90,
                    "headline": "Unused helper parameter",
                    "why": "Parameter is never read.",
                    "action": "Remove the parameter.",
                    "file_path": "src/a/very_long_filename_target.rs",
                    "line_start": 10,
                    "line_end": 12,
                    "suggested_diff": "-fn f(x: i32)\n+fn f()",
                    "trace": ["Note."],
                },
                {
                    "severity": "blocking",
                    "confidence": 96,
                    "headline": "Unchecked slice index",
                    "why": "Panics on empty input.",
                    "details": ["Callers can pass an empty slice."],
                    "action": "Return early when empty.",
                    "rule_reference": "rust_best_practices/panics",
                    "rule_link": "file:///repo/.agents/skills/r/SKILL.md#L1-L2",
                    "file_path": "src/a/b.rs",
                    "line_start": 5,
                    "trace": ["Run: `fx test foo_test`"],
                    "trace_title": "Reproduction",
                },
            ],
        }
        expected = """\
## Code review

❌ **Changes requested** (1 blocking, 1 suggestion)

Reviewed library changes.

- **Scope**: Working copy
- **Effort**: Standard (challenger kept 2 of 4 Round-1 draft findings)
- **Skills evaluated**: general

---

### Blocking issues

1. **Unchecked slice index** ([a/b.rs:5](file:///repo/src/a/b.rs#L5))

   **🚨 Blocking** (Confidence: 96%) ([**rust_best_practices/panics**](file:///repo/.agents/skills/r/SKILL.md#L1-L2))

   - **Why it matters:** Panics on empty input.
     - Callers can pass an empty slice.
   - **Action:** Return early when empty.

   <details>
   <summary>Reproduction</summary>

   - Run: `fx test foo_test`
   </details>

---

### Suggestions

2. **Unused helper parameter** ([very_long_filename_target.rs:10-12](file:///repo/src/a/very_long_filename_target.rs#L10-L12))

   **💡 Suggestion** (Confidence: 90%)

   - **Why it matters:** Parameter is never read.
   - **Action:** Remove the parameter.

   ```diff
   -fn f(x: i32)
   +fn f()
   ```

   <details>
   <summary>Supporting details</summary>

   - Note.
   </details>
"""
        self.assertEqual(render_report.render(payload, "/repo"), expected)
        self.assertSequenceEqual(
            [f["finding_id"] for f in payload["findings"]], ("1", "2")
        )

    def test_double_digit_finding_indent(self):
        findings = [
            {"severity": "suggestion", "headline": f"Item {i}", "why": "Why."}
            for i in range(1, 11)
        ]
        rendered = render_report.render(
            {"decision": "approved_with_suggestions", "findings": findings}
        )
        self.assertIn(
            "\n10. **Item 10**\n\n"
            "    **💡 Suggestion**\n\n"
            "    - **Why it matters:** Why.\n",
            rendered,
        )

    def test_backticks_in_diff_and_short_location_link(self):
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
1. [docs/a.md:3](file:///repo/docs/a.md#L3)

   **💡 Suggestion**

   Fix the example.

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

    def test_multiline_and_escaped_structured_fields(self):
        payload = {
            "decision": "changes_requested",
            "scope": "repo_a\n  repo_b",
            "findings": [
                {
                    "severity": "blocking",
                    "headline": "Split\n  title",
                    "why": "Line 1.\nLine 2.\n\nLine 3.",
                    "details": ["- Detail 1.\nDetail 2."],
                    "action": "Step 1.\nStep 2.",
                    "file_path": "   ",
                    "trace": [
                        "1. First step\ncontinues",
                        "2. Second step",
                        "- Run:\nfx test",
                    ],
                    "trace_title": "Callers of\n  Foo<T>",
                }
            ],
        }
        expected = """\
## Code review

❌ **Changes requested** (1 blocking, 0 suggestions)

- **Scope**: repo_a repo_b

---

### Blocking issues

1. **Split title**

   **🚨 Blocking**

   - **Why it matters:** Line 1.
     Line 2.

     Line 3.
     - Detail 1.
       Detail 2.
   - **Action:** Step 1.
     Step 2.

   <details>
   <summary>Callers of Foo&lt;T&gt;</summary>

   1. First step
      continues
   2. Second step

   - Run:
     fx test
   </details>
"""
        self.assertEqual(render_report.render(payload, "/repo"), expected)
        self.assertNotIn("file_path", payload["findings"][0])

    def test_package_anchoring_and_unique_suffix_disambiguation(self):
        with tempfile.TemporaryDirectory() as tmp:
            for marker in (
                "src/BUILD.gn",
                "src/lib/pkg_a/Cargo.toml",
                "src/drivers/drv_b/BUILD.gn",
                "docs/BUILD.gn",
                ".agents/skills/review/SKILL.md",
            ):
                full = os.path.join(tmp, marker)
                os.makedirs(os.path.dirname(full), exist_ok=True)
                with open(full, "w", encoding="utf-8"):
                    pass
            payload = {
                "decision": "changes_requested",
                "reviewed_files": [{"file_path": "crates/two/src/lib.rs"}],
                "findings": [
                    {
                        "severity": "blocking",
                        "headline": "Crate file",
                        "why": "Reason.",
                        "file_path": "src/lib/pkg_a/src/transport.rs",
                        "line_start": 10,
                    },
                    {
                        "severity": "blocking",
                        "headline": "Driver test",
                        "why": "Reason.",
                        "file_path": "src/drivers/drv_b/tests/bad_tests.cc",
                        "line_start": 20,
                    },
                    {
                        "severity": "blocking",
                        "headline": "Skill script",
                        "why": "Reason.",
                        "file_path": ".agents/skills/review/scripts/run.py",
                        "line_start": 5,
                    },
                    {
                        "severity": "blocking",
                        "headline": "Deep doc ignores distant umbrella BUILD",
                        "why": "Reason.",
                        "file_path": "docs/dev/lang/rust/guide.md",
                        "line_start": 3,
                    },
                    {
                        "severity": "blocking",
                        "headline": "Colliding path disambiguated against reviewed_files",
                        "why": "Reason.",
                        "file_path": "crates/one/src/lib.rs",
                        "line_start": 8,
                    },
                ],
            }
            rendered = render_report.render(payload, tmp)
        self.assertIn("[review/scripts/run.py:5](", rendered)
        self.assertIn("[one/src/lib.rs:8](", rendered)
        self.assertIn("[rust/guide.md:3](", rendered)
        self.assertIn("[drv_b/tests/bad_tests.cc:20](", rendered)
        self.assertIn("[pkg_a/src/transport.rs:10](", rendered)

    def test_structured_validation_errors(self):
        missing_headline_cases = (
            {"severity": "blocking", "headline": None, "why": "y"},
            {"severity": "blocking", "why": "y"},
            {"severity": "blocking", "why": None},
            {"severity": "blocking", "action": "z"},
            {"severity": "blocking", "comment": "legacy", "why": "y"},
        )
        for finding in missing_headline_cases:
            with self.subTest(finding=finding):
                with self.assertRaisesRegex(
                    render_report.ReportError, r"findings\[0\]\.headline"
                ):
                    render_report.render(
                        {"decision": "changes_requested", "findings": [finding]}
                    )

        base = {"severity": "blocking", "headline": "x", "why": "y"}
        bad_findings = (
            {**base, "headline": " "},
            {**base, "headline": 1},
            {**base, "pre_existing": True, "headline": "[Pre-existing]: "},
            {"severity": "blocking", "headline": "x"},
            {**base, "why": " "},
            {**base, "details": "x"},
            {**base, "details": ["x", " "]},
            {**base, "trace": [1]},
            {**base, "action": 1},
            {**base, "trace_title": []},
        )
        for finding in bad_findings:
            with self.subTest(finding=finding):
                with self.assertRaises(render_report.ReportError):
                    render_report.render(
                        {"decision": "changes_requested", "findings": [finding]}
                    )
        for bad_skills in ("general", [123]):
            with self.subTest(agent_skills=bad_skills):
                with self.assertRaisesRegex(
                    render_report.ReportError, "agent_skills"
                ):
                    render_report.render(
                        {"decision": "approved", "agent_skills": bad_skills}
                    )

    def test_challenger_counts_rendering_and_validation(self):
        rendered = render_report.render(
            {
                "decision": "approved",
                "effort": "standard",
                "challenger_kept": 0,
                "challenger_total": 3,
            }
        )
        self.assertIn(
            "- **Effort**: Standard (challenger kept 0 of 3 Round-1 draft findings)",
            rendered,
        )
        skipped_r2 = render_report.render(
            {
                "decision": "approved",
                "effort": "standard",
                "challenger_kept": 0,
                "challenger_total": 0,
            }
        )
        self.assertIn("- **Effort**: Standard\n", skipped_r2)
        self.assertNotIn("challenger kept", skipped_r2)
        quick_mode = render_report.render(
            {
                "decision": "approved",
                "effort": "quick",
                "challenger_kept": 1,
                "challenger_total": 2,
            }
        )
        self.assertNotIn("challenger kept", quick_mode)
        bad_counts = (
            (1, None),
            (None, 3),
            (4, 3),
            (-1, 3),
            (True, 3),
            (1, "3"),
        )
        for kept, total in bad_counts:
            payload = {"decision": "approved"}
            if kept is not None:
                payload["challenger_kept"] = kept
            if total is not None:
                payload["challenger_total"] = total
            with self.subTest(payload=payload):
                with self.assertRaises(render_report.ReportError):
                    render_report.render(payload)

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
                    "headline": "Indexing an empty slice panics",
                    "why": "Panics on empty slice.",
                    "rule_reference": "rust_best_practices/panics",
                    "file_path": "src/lib.rs",
                    "line_start": 10,
                }
            ],
        }
        rendered = render_report.render(payload, "/repo")
        self.assertIn(
            "- **Skipped files**: 2 files matched skip globs (`Cargo.lock`,"
            " `docs/a.png`)",
            rendered,
        )
        self.assertIn(
            "   **🚨 Blocking** (Confidence: 95%) (*rust_best_practices/panics*)",
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
        self.assertIn(
            "🟢 **Approved with minor suggestions** (0 blocking, 1 suggestion)",
            rendered,
        )
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
                    "headline": "[Pre-existing]: Legacy race on shutdown",
                    "why": "The background thread accesses freed state.",
                    "file_path": "src/lib.rs",
                    "line_start": 25,
                },
            ],
        }
        rendered = render_report.render(payload, "/repo")
        self.assertIn(
            "🟢 **Approved with minor suggestions** (0 blocking, 2 suggestions)",
            rendered,
        )
        self.assertIn(
            "**💡 Suggestion** [Pre-existing] (Confidence: 88%)"
            " (*general/reliability*)",
            rendered,
        )
        self.assertIn("   Unchecked error in moved helper.", rendered)
        self.assertIn(
            "2. **Legacy race on shutdown** ([src/lib.rs:25](file:///repo/src/lib.rs#L25))\n\n"
            "   **💡 Suggestion** [Pre-existing] (Confidence: 85%)",
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
        self.assertIn(
            "🟢 **Approved with minor suggestions** (0 blocking, 1 suggestion)",
            rendered,
        )
        self.assertIn("**💡 Suggestion** [Pre-existing]", rendered)
        self.assertIn("   Old helper bug.", rendered)

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
        self.assertIn(
            "❌ **Changes requested** (1 blocking, 1 suggestion)", rendered_mixed
        )
        self.assertEqual(mixed["findings"][0]["severity"], "blocking")
        self.assertEqual(mixed["findings"][1]["severity"], "suggestion")
        self.assertIn(
            "   [Pre-existing] New bug with literal prefix.", rendered_mixed
        )

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
            "🟢 **Approved**",
            render_report.render(filtered, "/repo", min_confidence=80),
        )

        # not_applicable stays not_applicable.
        na = {"decision": "not_applicable", "findings": []}
        self.assertIn(
            "ℹ️ **Not applicable**", render_report.render(na, "/repo")
        )

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
        self.assertIn(
            "❌ **Changes requested** (1 blocking, 0 suggestions)", rendered
        )
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
            self.assertIn(
                "🟢 **Approved with minor suggestions** (0 blocking, 1 suggestion)",
                out.getvalue(),
            )
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

    def test_simplify_mode_downgrades_blocking_and_renders_header(self):
        payload = {
            "decision": "changes_requested",
            "scope": "fuchsia.git @ 60ba9289d63",
            "effort": "SIMPLIFY",
            "agent_skills": ["code-simplification"],
            "findings": [
                {
                    "severity": "blocking",
                    "confidence": 91,
                    "headline": (
                        "Single-implementation trait can use concrete struct"
                    ),
                    "why": "Only one type implements PacketBuilder, so the trait adds indirection.",
                    "rule_reference": "code-simplification/scope",
                    "rule_link": (
                        "file:///repo/.agents/skills/code-simplification/"
                        "SKILL.md#L40-L50"
                    ),
                    "file_path": "src/foo/lib.rs",
                    "line_start": 14,
                    "code_snippet": "trait PacketBuilder {",
                },
                {
                    "severity": "suggestion",
                    "confidence": 72,
                    "comment": "Low-confidence stylistic nit.",
                    "rule_reference": "code-simplification/clarity",
                    "file_path": "src/foo/lib.rs",
                    "line_start": 30,
                },
            ],
        }
        rendered = render_report.render(payload, "/repo", min_confidence=80)
        self.assertIn("- **Effort**: Simplify", rendered)
        self.assertIn("- **Skills evaluated**: code-simplification", rendered)
        self.assertIn(
            "🟢 **Approved with minor suggestions** (0 blocking, 1 suggestion)",
            rendered,
        )
        self.assertNotIn("**Changes requested**", rendered)
        self.assertNotIn("Low-confidence stylistic nit.", rendered)
        self.assertEqual(len(payload["findings"]), 1)
        self.assertEqual(payload["findings"][0]["severity"], "suggestion")
        self.assertIn(
            "**💡 Suggestion** (Confidence: 91%)"
            " ([**code-simplification/scope**]"
            "(file:///repo/.agents/skills/code-simplification/SKILL.md#L40-L50))",
            rendered,
        )


if __name__ == "__main__":
    unittest.main()
