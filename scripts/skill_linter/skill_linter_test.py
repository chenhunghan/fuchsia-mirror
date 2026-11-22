#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import io
import json
import logging
import os
import shutil
import sys
import tempfile
import unittest
from typing import Any
from unittest import mock

import skill_linter


class TestSkillLinter(unittest.TestCase):
    def test_suggest_valid_name(self) -> None:
        self.assertEqual(
            skill_linter._suggest_valid_name("Valid-Name"), "valid-name"
        )
        self.assertEqual(
            skill_linter._suggest_valid_name("Name_With_Underscores"),
            "name-with-underscores",
        )
        self.assertEqual(
            skill_linter._suggest_valid_name("Invalid@Chars!"), "invalidchars"
        )
        self.assertEqual(skill_linter._suggest_valid_name("a" * 100), "a" * 64)

    def test_pre_process(self) -> None:
        # Test list item indentation standardization
        text = "-   item"
        self.assertEqual(skill_linter._pre_process(text, 4), "- item")

        text = "1.   item"
        self.assertEqual(skill_linter._pre_process(text, 2), "1. item")
        self.assertEqual(skill_linter._pre_process(text, 4), "1.  item")

        # Test that code blocks are ignored
        text = '```diff\ndeps = [\n-  "//src/lib/ddk"\n]\n```'
        self.assertEqual(skill_linter._pre_process(text, 4), text)

        text = "```markdown\n-   Item 1\n*   Item 2\n+   Item 3\n1.   Numbered\n```"
        self.assertEqual(skill_linter._pre_process(text, 4), text)

        text = "```markdown\n# Header 1\n## Header 2\n```"
        self.assertEqual(skill_linter._pre_process(text, 4), text)

    def test_pre_process_nested_list_under_numbered_list(self) -> None:
        # Test that 3-space nested bullets, 5-space continuations, and 3-space code
        # blocks under numbered lists are normalized to 4, 6, and 4 spaces.
        text = (
            "1. First item\n"
            "   * Child bullet with a longer description that wraps onto the"
            " next line to verify indentation\n"
            "     Continuation line\n"
            "   ```shell\n"
            "   command\n"
            "   ```\n"
            "   Continuation paragraph\n"
            "2. Second item\n"
        )
        expected_4 = (
            "1.  First item\n"
            "    * Child bullet with a longer description that wraps onto the"
            " next line to verify indentation\n"
            "      Continuation line\n"
            "    ```shell\n"
            "    command\n"
            "    ```\n"
            "    Continuation paragraph\n"
            "2.  Second item\n"
        )
        self.assertEqual(skill_linter._pre_process(text, 4), expected_4)

        # Test that already properly indented lists are preserved.
        self.assertEqual(skill_linter._pre_process(expected_4, 4), expected_4)

        # Test formatting end-to-end with format_markdown
        formatted = skill_linter.format_markdown(text, tab_size=4)
        self.assertIn("    * Child bullet", formatted)
        self.assertIn("      verify indentation", formatted)
        self.assertIn("    ```shell", formatted)
        self.assertIn("    Continuation paragraph", formatted)

    def test_pre_process_code_blocks_inside_list(self) -> None:
        # Test that code blocks inside numbered lists are properly indented.
        text = (
            "1. Step One:\n"
            "   ```bash\n"
            "   ffx target list\n"
            "   fx status\n"
            "   ```\n"
            "2. Step Two:\n"
            "   ```bash\n"
            "   fx add-test //src/tests:foo\n"
            "   fx build\n"
            "   ```\n"
        )
        expected = (
            "1.  Step One:\n"
            "    ```bash\n"
            "    ffx target list\n"
            "    fx status\n"
            "    ```\n"
            "2.  Step Two:\n"
            "    ```bash\n"
            "    fx add-test //src/tests:foo\n"
            "    fx build\n"
            "    ```\n"
        )
        self.assertEqual(skill_linter._pre_process(text, 4), expected)
        formatted = skill_linter.format_markdown(text, tab_size=4)
        self.assertIn("    ```bash\n    ffx target list", formatted)
        self.assertIn("    ```bash\n    fx add-test", formatted)

    def test_post_process(self) -> None:
        # Test trailing whitespace removal and newlines
        text = "line with spaces    \nline 2"
        self.assertEqual(
            skill_linter._post_process(text), "line with spaces  \nline 2\n"
        )

        text = "line\n\n\nline2"
        self.assertEqual(skill_linter._post_process(text), "line\n\nline2\n")

    def test_validate_name(self) -> None:
        meta = {"name": "valid-name"}
        errors, fixes = skill_linter._validate_name(meta, fixit=False)
        self.assertEqual(errors, [])
        self.assertEqual(fixes, [])

        meta = {"name": "Invalid Name"}
        errors, fixes = skill_linter._validate_name(meta, fixit=False)
        self.assertTrue(len(errors) > 0)

        meta = {"name": "Invalid Name"}
        errors, fixes = skill_linter._validate_name(meta, fixit=True)
        self.assertEqual(meta["name"], "invalidname")
        self.assertTrue(len(fixes) > 0)

    def test_validate_description(self) -> None:
        meta = {"description": "Valid description"}
        errors, fixes = skill_linter._validate_description(meta, fixit=False)
        self.assertEqual(errors, [])
        self.assertEqual(fixes, [])

        meta = {"description": "Invalid description with <tags>"}
        errors, fixes = skill_linter._validate_description(meta, fixit=False)
        self.assertTrue(len(errors) > 0)

        meta = {"description": "Invalid description with <tags>"}
        errors, fixes = skill_linter._validate_description(meta, fixit=True)
        self.assertEqual(meta["description"], "Invalid description with")
        self.assertTrue(len(fixes) > 0)

    def test_format_markdown_paragraphs(self) -> None:
        # Test paragraph wrapping
        text = "This is a long paragraph that should be wrapped to fit within the eighty character limit that is imposed by the linter."
        formatted = skill_linter.format_markdown(text, width=40)
        expected = (
            "This is a long paragraph that should be\n"
            "wrapped to fit within the eighty\n"
            "character limit that is imposed by the\n"
            "linter.\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_lists(self) -> None:
        # Test list formatting and indentation
        text = "- Item 1\n- Item 2 with a very long description that should wrap correctly under the marker."
        formatted = skill_linter.format_markdown(text, width=20)
        # Note: textwrap behavior might differ slightly from hand-calculated expected
        expected = (
            "- Item 1\n"
            "- Item 2 with a very\n"
            "  long description\n"
            "  that should wrap\n"
            "  correctly under\n"
            "  the marker.\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_code_blocks(self) -> None:
        # Test that code blocks are preserved
        text = "Para before.\n\n```python\ndef foo():\n    pass\n```\n\nPara after."
        formatted = skill_linter.format_markdown(text)
        self.assertIn("```python\ndef foo():\n    pass\n```", formatted)

        # Test complex markdown structures inside a code block
        text = "```markdown\n-   item 1\n\n| col 1 | col 2 |\n|---|---|\n| a | b |\n\n# Header\n```"
        formatted = skill_linter.format_markdown(text)
        self.assertEqual(formatted, text + "\n")

        # Test preservation of purely whitespace lines in code blocks
        text = "```rust\nfn main() {\n    let x = 5;\n    \n    println!(x);\n}\n```"
        formatted = skill_linter.format_markdown(text)
        self.assertEqual(formatted, text + "\n")

    def test_format_markdown_tables(self) -> None:
        # Test that tables are preserved and not wrapped
        text = "| Header 1 | Header 2 |\n| --- | --- |\n| Cell 1 | Cell 2 |"
        formatted = skill_linter.format_markdown(text, width=10)
        # Width 10 should normally wrap lines, but tables should be skipped
        self.assertEqual(formatted, text + "\n")

    def test_format_markdown_multiline_tables(self) -> None:
        # Test that multi-line tables with colon row continuations are preserved
        text = (
            "| Situation              | Action"
            "                                            |\n"
            "| :--------------------- |"
            " :------------------------------------------------ |\n"
            "| **P0 Bug**             | Assign an owner IMMEDIATELY. Ensure"
            " it's Accepted |\n"
            ":                        : ASAP."
            "                                             :\n"
            "| **Semaphore Hang**     | Use"
            " `fuchsia-diagnose-vksemaphore-hangs` to       |\n"
            ":                        : identify client KOID. If display Vsync"
            " stalled,   :\n"
            ":                        : move to `componentid\\:1335242`   "
            "                 :\n"
            ":                        : (`tq-display-bugs@`).            "
            "                 :\n"
            "| **Timekeeping**        | Assign to `fmil@`."
            "                                |\n"
        )
        formatted = skill_linter.format_markdown(text, width=10)
        self.assertEqual(formatted, text)
        _, issues = skill_linter.format_markdown_with_issues(text, width=80)
        self.assertEqual(issues, [])

    def test_format_markdown_headers(self) -> None:
        # Test that headers are preserved
        text = "# Header 1\n\n## Header 2\n\nContent."
        formatted = skill_linter.format_markdown(text)
        self.assertIn("# Header 1\n", formatted)
        self.assertIn("## Header 2\n", formatted)

    def test_format_markdown_hard_breaks(self) -> None:
        # Test that Markdown hard line breaks (two spaces) are preserved
        text = "Line 1  \nLine 2"
        formatted = skill_linter.format_markdown(text)
        self.assertEqual(formatted, "Line 1  \nLine 2\n")

    def test_format_markdown_with_issues_table_boundaries(self) -> None:
        # Test that lines immediately following a table are correctly validated for width
        text = "| Header |\n| --- |\n| Cell |\nThis is a very long text line immediately following the table with no newline."
        _, issues = skill_linter.format_markdown_with_issues(text, width=20)
        self.assertIn("lines exceeding 20 characters", issues)

    def test_format_markdown_with_issues_tables_ignored(self) -> None:
        text = "| Extremely long table header that exceeds eighty characters |\n| --- |\n| Cell without trailing spaces |\n"
        _, issues = skill_linter.format_markdown_with_issues(text, width=20)
        self.assertEqual(issues, [])

    def test_format_markdown_with_issues_code_blocks_ignored(self) -> None:
        text = "```python\ndef foo():\n\n\n    print('Trailing space here:   ')\n```\n"
        _, issues = skill_linter.format_markdown_with_issues(text, width=80)
        self.assertEqual(issues, [])

    def test_format_markdown_with_issues_normal_text(self) -> None:
        text = "Normal paragraph.\n\n\nAnother normal paragraph with trailing space    \n"
        _, issues = skill_linter.format_markdown_with_issues(text, width=80)
        self.assertIn("consecutive empty lines", issues)
        self.assertIn("trailing whitespace", issues)

    def test_format_markdown_missing_empty_line_before_list(self) -> None:
        # Paragraph followed immediately by numbered list without empty line
        text = (
            "To configure your build, complete the following steps:\n"
            "1. Select your target device.\n"
            "2. Build the package.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = (
            "To configure your build, complete the following steps:\n\n"
            "1.  Select your target device.\n"
            "2.  Build the package.\n"
        )
        self.assertEqual(formatted, expected)

        # Paragraph followed immediately by bullet list without empty line
        text_bullet = (
            "Available components:\n" "* Component A\n" "* Component B\n"
        )
        (
            formatted_bullet,
            issues_bullet,
        ) = skill_linter.format_markdown_with_issues(text_bullet)
        self.assertIn("missing empty line before list", issues_bullet)
        expected_bullet = (
            "Available components:\n\n" "* Component A\n" "* Component B\n"
        )
        self.assertEqual(formatted_bullet, expected_bullet)

    def test_format_markdown_missing_empty_line_before_list_already_spaced(
        self,
    ) -> None:
        text = (
            "To configure your build, complete the following steps:\n\n"
            "1.  Select your target device.\n"
            "2.  Build the package.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertEqual(issues, [])
        self.assertEqual(formatted, text)

    def test_format_markdown_missing_empty_line_before_list_after_heading(
        self,
    ) -> None:
        text = "### Steps\n1. First step.\n2. Second step.\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = "### Steps\n\n1.  First step.\n2.  Second step.\n"
        self.assertEqual(formatted, expected)

    def test_format_markdown_missing_empty_line_before_list_preserves_tight_lists(
        self,
    ) -> None:
        text = (
            "Introductory text:\n\n"
            "1.  Step 1\n"
            "    * Sub-step a\n"
            "    * Sub-step b\n"
            "2.  Step 2\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertEqual(issues, [])
        self.assertEqual(formatted, text)

    def test_format_markdown_missing_empty_line_around_code_block(
        self,
    ) -> None:
        text = (
            "Paragraph before code block.\n"
            "```python\n"
            "def foo():\n"
            "    pass\n"
            "```\n"
            "Paragraph after code block.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before code block", issues)
        self.assertIn("missing empty line after code block", issues)
        expected = (
            "Paragraph before code block.\n\n"
            "```python\n"
            "def foo():\n"
            "    pass\n"
            "```\n\n"
            "Paragraph after code block.\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_code_block_already_spaced(self) -> None:
        text = (
            "Paragraph before code block.\n\n"
            "```python\n"
            "def foo():\n"
            "    pass\n"
            "```\n\n"
            "Paragraph after code block.\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertEqual(issues, [])
        self.assertEqual(formatted, text)

    def test_format_markdown_missing_empty_line_after_list(self) -> None:
        # Headings and link reference definitions directly after a list.
        text = (
            "- Item with [a link][ref]\n"
            "[ref]: https://fuchsia.dev\n"
            "\n"
            "- Another item\n"
            "## Next section\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line after list", issues)
        expected = (
            "- Item with [a link][ref]\n"
            "\n"
            "[ref]: https://fuchsia.dev\n"
            "\n"
            "- Another item\n"
            "\n"
            "## Next section\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_lazy_continuation_keeps_list(self) -> None:
        # A non-indented line directly after a list item is a lazy
        # continuation, so no empty line is inserted before the next item.
        text = "Intro:\n\n- First item\nlazy continuation\n- Second item\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertNotIn("missing empty line before list", issues)
        expected = "Intro:\n\n- First item lazy continuation\n- Second item\n"
        self.assertEqual(formatted, expected)

    def test_format_markdown_list_after_new_paragraph(self) -> None:
        # A non-indented paragraph after an empty line ends the list, so a
        # following list needs a new empty line.
        text = "- First list\n\nA new paragraph:\n- Second list\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = "- First list\n\nA new paragraph:\n\n- Second list\n"
        self.assertEqual(formatted, expected)

    def test_format_markdown_nested_list_after_item_text(self) -> None:
        # A nested list following continuation text inside a list item.
        text = (
            "1.  Parent item:\n"
            "\n"
            "    Continuation paragraph:\n"
            "    - Child A\n"
            "    - Child B\n"
            "2.  Next parent\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before list", issues)
        expected = (
            "1.  Parent item:\n"
            "\n"
            "    Continuation paragraph:\n"
            "\n"
            "    - Child A\n"
            "    - Child B\n"
            "2.  Next parent\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_code_block_inside_list_item(self) -> None:
        text = (
            "1.  Run the command:\n"
            "    ```bash\n"
            "    fx build\n"
            "    ```\n"
            "    Then continue with the next step.\n"
            "2.  Next step\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertIn("missing empty line before code block", issues)
        self.assertIn("missing empty line after code block", issues)
        # The next item follows a continuation paragraph rather than a list
        # item paragraph, so it is also separated by an empty line.
        self.assertIn("missing empty line before list", issues)
        expected = (
            "1.  Run the command:\n"
            "\n"
            "    ```bash\n"
            "    fx build\n"
            "    ```\n"
            "\n"
            "    Then continue with the next step.\n"
            "\n"
            "2.  Next step\n"
        )
        self.assertEqual(formatted, expected)

    def test_format_markdown_whitespace_only_line_counts_as_empty(
        self,
    ) -> None:
        text = "Intro paragraph.\n  \n- Item\n"
        formatted, issues = skill_linter.format_markdown_with_issues(text)
        self.assertNotIn("missing empty line before list", issues)
        self.assertNotIn("\n\n\n", formatted)

    def test_format_markdown_spacing_rules_are_idempotent(self) -> None:
        text = (
            "Intro:\n"
            "- Item A\n"
            "    ```bash\n"
            "    echo a\n"
            "    ```\n"
            "    More text.\n"
            "- Item B\n"
            "### Heading\n"
            "Text before code:\n"
            "```\n"
            "code\n"
            "```\n"
            "Text after code.\n"
        )
        once, _ = skill_linter.format_markdown_with_issues(text)
        twice, issues = skill_linter.format_markdown_with_issues(once)
        self.assertEqual(once, twice)
        self.assertEqual(issues, [])

    def test_format_markdown_link_reference_definitions_not_merged(
        self,
    ) -> None:
        text = (
            "Paragraph before links.\n\n"
            "[ref-a]: https://example.com/a\n"
            "[ref-b]: https://example.com/b\n"
            "[ref-c]: https://example.com/very/long/url/that/exceeds/the/specified/maximum/width/and/should/not/be/flagged/as/long/line\n"
        )
        formatted, issues = skill_linter.format_markdown_with_issues(
            text, width=40
        )
        self.assertEqual(issues, [])
        self.assertIn("[ref-a]: https://example.com/a\n", formatted)
        self.assertIn("[ref-b]: https://example.com/b\n", formatted)
        self.assertIn(
            "[ref-c]: https://example.com/very/long/url/that/exceeds/the/specified/maximum/width/and/should/not/be/flagged/as/long/line\n",
            formatted,
        )
        # Ensure definitions are not concatenated on the same line
        self.assertNotIn("[ref-a]: https://example.com/a [ref-b]:", formatted)

    def test_cli_modes_and_directory_traversal(self) -> None:
        temp_dir = tempfile.mkdtemp()
        try:
            # Create two skill subdirectories: clean-skill and dirty-skill
            clean_dir = os.path.join(temp_dir, "clean-skill")
            dirty_dir = os.path.join(temp_dir, "dirty-skill")
            os.makedirs(clean_dir)
            os.makedirs(dirty_dir)

            clean_skill = os.path.join(clean_dir, "SKILL.md")
            with open(clean_skill, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: clean-skill\n"
                    "description: A completely valid skill.\n"
                    "---\n\n"
                    "# Overview\n\n"
                    "Valid skill content.\n"
                )

            # dirty-skill has BOTH a formatting issue (single trailing space + consecutive empty lines)
            # AND a description error that cannot be auto-fixed (lone "<")
            dirty_skill = os.path.join(dirty_dir, "SKILL.md")
            with open(dirty_skill, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: dirty-skill\n"
                    "description: Compare a < b values.\n"
                    "---\n\n"
                    "# Overview \n\n\n"
                    "Run on the target.\n"
                )

            # 1. Directory traversal in check-only mode
            code, findings = skill_linter.do_lint(
                root_path=temp_dir,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
            )
            self.assertEqual(code, 1)

            # 2. JSON mode (--suggest-fix-in-json) on parent directory
            code_json, findings_json = skill_linter.do_lint(
                root_path=temp_dir,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
            )
            self.assertEqual(code_json, 0)
            self.assertEqual(len(findings_json), 1)
            finding = findings_json[0]
            self.assertEqual(finding["level"], "error")
            self.assertIn("angle brackets", finding["message"])
            self.assertIn("replacements", finding)
            # Replacement should have fixed the triple newline & trailing space
            self.assertNotIn("# Overview \n\n\n", finding["replacements"][0])

            # 3. --suggest-fix (stdout mode)
            captured = io.StringIO()
            with mock.patch("sys.stdout", captured):
                code_stdout, _ = skill_linter.do_lint(
                    root_path=dirty_skill,
                    fixit=True,
                    stdout_mode=True,
                    suggest_fix_in_json_mode=False,
                )
            self.assertEqual(code_stdout, 1)
            self.assertIn(
                "# Overview\n\nRun on the target.\n",
                captured.getvalue(),
            )

            # 4. --fixit mode writes formatting fixes to disk while still returning exit code 1
            code_fixit, _ = skill_linter.do_lint(
                root_path=dirty_skill,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
            )
            self.assertEqual(code_fixit, 1)
            with open(dirty_skill, "r", encoding="utf-8") as f:
                updated_disk = f.read()
            self.assertNotIn("# Overview \n\n\n", updated_disk)
        finally:
            shutil.rmtree(temp_dir)

    def test_generate_frontmatter_lines_preserves_nested_dicts_and_lists(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            with open(skill_path, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: test-skill\n"
                    "description: A valid skill.\n"
                    "metadata:\n"
                    "  hardware:\n"
                    "    board: x64\n"
                    "  targets:\n"
                    "    - name: emu\n"
                    "      arch: arm64\n"
                    "---\n\n"
                    "# Overview \n\n\n"
                    "Body.\n"
                )
            code, _ = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=True,
                stdout_mode=False,
                suggest_fix_in_json_mode=False,
            )
            self.assertEqual(code, 0)
            with open(skill_path, "r", encoding="utf-8") as f:
                updated = f.read()
            self.assertNotIn("{'hardware':", updated)
            meta, _ = skill_linter._parse_skill_frontmatter(updated)
            self.assertEqual(
                meta["metadata"],
                {
                    "hardware": {"board": "x64"},
                    "targets": [{"name": "emu", "arch": "arm64"}],
                },
            )

    def test_format_code_hint_gated_on_autofixes(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            content = (
                "---\n"
                "name: perfectly-formatted\n"
                "description: Compare a < b values.\n"
                "---\n\n"
                "# Title\n\n"
                "Well formatted body.\n"
            )
            with open(skill_path, "w") as f:
                f.write(content)

            # The lone angle bracket in the description is an error that has
            # no autofix, so the format-code hint must not be shown.

            # CLI mode
            logger = logging.getLogger()
            with self.assertLogs(logger, level="INFO") as log_context:
                exit_code, findings = skill_linter.lint_single_skill(
                    skill_file=skill_path,
                    fixit=False,
                    stdout_mode=False,
                    suggest_fix_in_json_mode=False,
                )
            self.assertEqual(exit_code, 1)
            log_output = "\n".join(log_context.output)
            self.assertNotIn(
                "You can run fx format-code to fix these issues.", log_output
            )

            # JSON mode
            exit_code, findings = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
            )
            self.assertEqual(len(findings), 1)
            self.assertIn("angle brackets", findings[0]["message"])
            self.assertNotIn(
                "You can run fx format-code to fix these issues.",
                findings[0]["message"],
            )

    def test_duplicate_yaml_keys_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            with open(skill_path, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: dup-skill\n"
                    "description: First description.\n"
                    "description: Overwritten description.\n"
                    "---\n\n"
                    "# Overview\n"
                )
            code, findings = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
            )
            self.assertEqual(code, 0)
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["level"], "error")
            self.assertIn("duplicate YAML key", findings[0]["message"])

    def test_unexpected_frontmatter_key_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            with open(skill_path, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: bad-key\n"
                    "description: Valid.\n"
                    "unknown_field: 123\n"
                    "---\n\n"
                    "# Title\n"
                )
            _, findings = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
            )
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["level"], "error")
            self.assertIn("Unexpected frontmatter key", findings[0]["message"])

    def test_lint_skill_semantic_rules(self) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            # 1. Singular .agent/ typo, binary path in description, missing triggers,
            # README.md in skill dir, file:// URI link, broken local link, and invalid call_tool.
            bad_dir = os.path.join(
                repo_dir, "src", "foo", ".agent", "skills", "wrong-folder"
            )
            os.makedirs(bad_dir)
            with open(
                os.path.join(bad_dir, "README.md"), "w", encoding="utf-8"
            ) as f:
                f.write("# Extra readme\n")

            skill_md = os.path.join(bad_dir, "SKILL.md")
            with open(skill_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: foo-playbook\n"
                    "description: Runs /google/bin/releases/foo/bar directly.\n"
                    "---\n\n"
                    "# Overview\n\n"
                    "See [rubric](file://references/rubric.md) and"
                    " [missing](references/missing.md).\n"
                    "Path: /usr/local/google/home/alice/fuchsia/out.\n\n"
                    "```python\n"
                    'await call_tool("run_workflow", ScriptPath="foo.py")\n'
                    "```\n\n"
                    "```bash\n"
                    "fx workflow run foo.py\n"
                    "```\n"
                )

            findings = skill_linter._lint_customization_paths(
                (skill_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            messages = "\n".join(fd["message"] for fd in findings)
            self.assertIn('uses singular ".agent/"', messages)
            self.assertIn("does not match YAML skill name", messages)
            self.assertIn("contains filler words", messages)
            self.assertIn("violating Progressive Disclosure", messages)
            self.assertIn("explicit positive trigger", messages)
            self.assertIn("explicit negative scope boundary", messages)
            self.assertIn('Skill directory contains "README.md"', messages)
            self.assertIn("Invalid file URI link", messages)
            self.assertIn("Broken local link", messages)
            self.assertIn("Machine-local path", messages)
            self.assertIn('call_tool("run_workflow"', messages)
            self.assertIn('"fx workflow" is not a valid', messages)

    def test_lint_skill_clean_passes_with_zero_findings(self) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            skill_dir = os.path.join(
                repo_dir, "src", "foo", ".agents", "skills", "audit-foo"
            )
            ref_dir = os.path.join(skill_dir, "references")
            os.makedirs(ref_dir)
            with open(
                os.path.join(ref_dir, "rubric.md"), "w", encoding="utf-8"
            ) as f:
                f.write("# Rubric\n")

            skill_md = os.path.join(skill_dir, "SKILL.md")
            with open(skill_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: audit-foo\n"
                    "description: >-\n"
                    "  Audits foo drivers against observability rules. Use when"
                    " auditing foo instrumentation. Don't use for live crash"
                    " debugging.\n"
                    "---\n\n"
                    "# Audit foo\n\n"
                    "Read the [foo rubric](references/rubric.md) and open"
                    " [panel](sidecar://foo/bar/).\n\n"
                    'Note in prose: never call `call_tool("run_workflow")`.\n'
                )

            findings = skill_linter._lint_customization_paths(
                (skill_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertSequenceEqual(findings, ())

    def test_lint_workflow_directory_checks(self) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            wf_dir = os.path.join(
                repo_dir, ".agents", "workflows", "triage-pipeline"
            )
            os.makedirs(wf_dir)

            manifest_path = os.path.join(wf_dir, "workflow.json")
            with open(manifest_path, "w", encoding="utf-8") as f:
                f.write(
                    "{\n"
                    '  "name": "wrong-name",\n'
                    '  "description": "Triage pipeline.",\n'
                    '  "extra_key": true,\n'
                    '  "args": {\n'
                    '    "type": "object",\n'
                    '    "required": ["missing_req"],\n'
                    '    "properties": {\n'
                    '      "target": {"type": "string"}\n'
                    "    }\n"
                    "  }\n"
                    "}\n"
                )

            script_path = os.path.join(wf_dir, "workflow.py")
            with open(script_path, "w", encoding="utf-8") as f:
                f.write(
                    "import subprocess\n"
                    'target = args.get("target")\n'
                    'undeclared = args.get("undeclared_arg")\n'
                    'await call_tool("run_command", CommandLine="ls")\n'
                    'await agent("hi", schema={"type": "string"},'
                    ' workspace="bad_ws", model="bad_model")\n'
                    'subprocess.run(["issues", "mutate", "create", "--title",'
                    ' "test"])\n'
                )

            readme_path = os.path.join(wf_dir, "README.md")
            with open(readme_path, "w", encoding="utf-8") as f:
                f.write(
                    "# Pipeline\n\n"
                    "/run-workflow wrong-name\n\n"
                    "| Argument | Type | Description |\n"
                    "| --- | --- | --- |\n"
                    "| target | string | Target |\n"
                )

            findings = skill_linter._lint_customization_paths(
                (manifest_path, script_path, readme_path),
                repo_root=repo_dir,
                fuchsia_root=repo_dir,
            )
            messages = "\n".join(fd["message"] for fd in findings)
            self.assertIn("Unexpected key(s) in workflow.json", messages)
            self.assertIn("must match enclosing directory", messages)
            self.assertIn('Required argument "missing_req"', messages)
            self.assertIn('missing a "description"', messages)
            self.assertIn('Argument "undeclared_arg" read from args', messages)
            self.assertIn('call_tool("run_command"', messages)
            self.assertIn('Invalid agent workspace="bad_ws"', messages)
            self.assertIn('Invalid agent model="bad_model"', messages)
            self.assertIn("external mutation commands", messages)
            self.assertIn("duplicates the arguments schema table", messages)
            self.assertIn("slash command directly", messages)

    def test_lint_discovery_json_and_vendor_dual_paths(self) -> None:
        with tempfile.TemporaryDirectory() as fuchsia_dir:
            vg_dir = os.path.join(fuchsia_dir, "vendor", "google")
            dot_agents = os.path.join(vg_dir, ".agents")
            skill_dir = os.path.join(vg_dir, "iris", "skills", "demo-skill")
            os.makedirs(dot_agents)
            os.makedirs(skill_dir)

            skills_json = os.path.join(dot_agents, "skills.json")
            with open(skills_json, "w", encoding="utf-8") as f:
                f.write(
                    "{\n"
                    '  "entries": [\n'
                    '    {"path": "iris/skills/demo-skill"},\n'
                    '    {"path": "iris/skills/nonexistent"}\n'
                    "  ]\n"
                    "}\n"
                )

            findings = skill_linter._lint_customization_paths(
                (skills_json,),
                repo_root=vg_dir,
                fuchsia_root=fuchsia_dir,
            )
            messages = "\n".join(fd["message"] for fd in findings)
            self.assertIn(
                'Configured entries path "iris/skills/nonexistent" does not exist',
                messages,
            )
            self.assertIn(
                'missing paired "vendor/google/iris/skills/demo-skill"',
                messages,
            )

    def test_markdown_links_whitespace_code_labels_hermetic_and_line_numbers(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            skill_dir = os.path.join(
                repo_dir, "src", "foo", ".agents", "skills", "link-check"
            )
            os.makedirs(skill_dir)
            skill_md = os.path.join(skill_dir, "SKILL.md")
            with open(skill_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"  # line 1
                    "name: link-check\n"  # line 2
                    "description: >-\n"  # line 3
                    "  Validates links. Use when testing links. Don't use for"
                    " builds.\n"  # line 4
                    "---\n"  # line 5
                    "\n"  # line 6
                    "# Link checks\n"  # line 7
                    "\n"  # line 8
                    "Empty whitespace target: [blank](   ) and"
                    " `[ignored](file://bad)`.\n"  # line 9
                    "Code label: [`MySymbol`](references/missing.md).\n"  # line 10
                    "Host root probe: [tmp](/tmp).\n"  # line 11
                )

            findings = skill_linter._lint_customization_paths(
                (skill_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            by_line = {fd["line"]: fd["message"] for fd in findings}
            self.assertSequenceEqual(sorted(by_line.keys()), (10, 11))
            self.assertIn(
                'Broken local link "references/missing.md"', by_line[10]
            )
            self.assertIn('Broken local link "/tmp"', by_line[11])

    def test_workflow_mutation_detection_ignores_docstrings_and_catches_sequences(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            wf_dir = os.path.join(
                repo_dir, ".agents", "workflows", "safe-doc-workflow"
            )
            os.makedirs(wf_dir)
            manifest_path = os.path.join(wf_dir, "workflow.json")
            with open(manifest_path, "w", encoding="utf-8") as f:
                f.write(
                    "{\n"
                    '  "name": "safe-doc-workflow",\n'
                    '  "description": "Reads state without mutating."\n'
                    "}\n"
                )

            script_path = os.path.join(wf_dir, "workflow.py")
            with open(script_path, "w", encoding="utf-8") as f:
                f.write(
                    '"""Does not run git push or jiri upload."""\n'
                    'await agent("Never run git push or issues mutate.")\n'
                )

            self.assertSequenceEqual(
                skill_linter._lint_customization_paths(
                    (wf_dir,), repo_root=repo_dir, fuchsia_root=repo_dir
                ),
                (),
            )

            with open(script_path, "w", encoding="utf-8") as f:
                f.write(
                    '"""Runs git push via sequence args."""\n'
                    "import subprocess\n"
                    'subprocess.run(["git", "push", "origin", "HEAD"])\n'
                )

            findings = skill_linter._lint_customization_paths(
                (wf_dir,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["line"], 3)
            self.assertIn("external mutation commands", findings[0]["message"])

    def test_discovery_json_skips_uncloned_vendor_on_public_checkout(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as public_fuchsia_dir:
            dot_agents = os.path.join(public_fuchsia_dir, ".agents")
            os.makedirs(os.path.join(dot_agents, "skills"))
            skills_json = os.path.join(dot_agents, "skills.json")
            with open(skills_json, "w", encoding="utf-8") as f:
                f.write(
                    "{\n"
                    '  "entries": [{"path": ".agents/skills"}],\n'
                    '  "inherits": [{"path":'
                    ' "vendor/google/.agents/skills.json"}]\n'
                    "}\n"
                )

            findings = skill_linter._lint_customization_paths(
                (skills_json,),
                repo_root=public_fuchsia_dir,
                fuchsia_root=public_fuchsia_dir,
            )
            self.assertSequenceEqual(findings, ())

    def test_unexpected_non_string_frontmatter_key_does_not_crash(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            skill_path = os.path.join(temp_dir, "SKILL.md")
            with open(skill_path, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: non-str-key\n"
                    "description: Valid.\n"
                    "123: foo\n"
                    "true: bar\n"
                    "---\n\n"
                    "# Title\n"
                )
            _, findings = skill_linter.lint_single_skill(
                skill_file=skill_path,
                fixit=False,
                stdout_mode=False,
                suggest_fix_in_json_mode=True,
            )
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["level"], "error")
            self.assertIn("Unexpected frontmatter key", findings[0]["message"])

    def test_skill_size_limits_and_legacy_allowlist(self) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            skill_dir = os.path.join(
                repo_dir, ".agents", "skills", "large-audit"
            )
            os.makedirs(skill_dir)
            skill_md = os.path.join(skill_dir, "SKILL.md")
            header = (
                "---\n"
                "name: large-audit\n"
                "description: Test limits. Use when testing. Don't use for"
                " prod.\n"
                "---\n\n"
            )

            with open(skill_md, "w", encoding="utf-8") as f:
                f.write(header + "\n".join("line" for _ in range(520)) + "\n")
            warn_findings = skill_linter._lint_customization_paths(
                (skill_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(warn_findings), 1)
            self.assertEqual(warn_findings[0]["level"], "warning")
            self.assertIn("soft limit", warn_findings[0]["message"])

            with open(skill_md, "w", encoding="utf-8") as f:
                f.write(header + "\n".join("line" for _ in range(820)) + "\n")
            err_findings = skill_linter._lint_customization_paths(
                (skill_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(err_findings), 1)
            self.assertEqual(err_findings[0]["level"], "error")
            self.assertIn("view_file ceiling", err_findings[0]["message"])

            legacy_dir = os.path.join(
                repo_dir, "zircon", "skills", "kernel-review"
            )
            os.makedirs(legacy_dir)
            legacy_md = os.path.join(legacy_dir, "SKILL.md")
            with open(legacy_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: kernel-review\n"
                    "description: Legacy. Use when testing. Don't use for"
                    " prod.\n"
                    "---\n\n" + "\n".join("line" for _ in range(820)) + "\n"
                )
            legacy_findings = skill_linter._lint_customization_paths(
                (legacy_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(legacy_findings), 1)
            self.assertEqual(legacy_findings[0]["level"], "warning")

    def test_workflow_json_types_library_traversal_subscript_dry_run_syntax(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            bad_wf = os.path.join(repo_dir, ".agents", "workflows", "bad-types")
            os.makedirs(bad_wf)
            with open(
                os.path.join(bad_wf, "workflow.json"), "w", encoding="utf-8"
            ) as f:
                json.dump(
                    {
                        "name": 123,
                        "description": None,
                        "library": "false",
                        "entry": 123,
                    },
                    f,
                )
            bad_findings = skill_linter._lint_customization_paths(
                (bad_wf,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            bad_msgs = "\n".join(fd["message"] for fd in bad_findings)
            self.assertIn('"name" must be a non-empty kebab-case', bad_msgs)
            self.assertIn('"library" must be a boolean', bad_msgs)
            self.assertIn('"description" cannot be empty', bad_msgs)
            self.assertIn('"entry" must be a non-empty string', bad_msgs)

            lib_wf = os.path.join(repo_dir, ".agents", "workflows", "lib-only")
            os.makedirs(lib_wf)
            with open(
                os.path.join(lib_wf, "workflow.json"), "w", encoding="utf-8"
            ) as f:
                json.dump(
                    {
                        "name": "lib-only",
                        "description": "Shared library.",
                        "library": True,
                    },
                    f,
                )
            self.assertSequenceEqual(
                skill_linter._lint_customization_paths(
                    (lib_wf,), repo_root=repo_dir, fuchsia_root=repo_dir
                ),
                (),
            )

            esc_wf = os.path.join(repo_dir, ".agents", "workflows", "escape-wf")
            os.makedirs(esc_wf)
            with open(
                os.path.join(esc_wf, "workflow.json"), "w", encoding="utf-8"
            ) as f:
                json.dump(
                    {
                        "name": "escape-wf",
                        "description": "Traversal check.",
                        "entry": "../evil.py",
                    },
                    f,
                )
            esc_findings = skill_linter._lint_customization_paths(
                (esc_wf,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(esc_findings), 1)
            self.assertIn("must stay inside", esc_findings[0]["message"])

            safe_wf = os.path.join(repo_dir, ".agents", "workflows", "safe-mut")
            os.makedirs(safe_wf)
            with open(
                os.path.join(safe_wf, "workflow.json"), "w", encoding="utf-8"
            ) as f:
                json.dump(
                    {
                        "name": "safe-mut",
                        "description": "Guarded mutation.",
                        "args": {
                            "type": "object",
                            "properties": {
                                "target": {
                                    "type": "string",
                                    "description": "Target.",
                                },
                                "dry_run": {
                                    "type": "boolean",
                                    "description": "Dry run.",
                                    "default": True,
                                },
                            },
                        },
                    },
                    f,
                )
            with open(
                os.path.join(safe_wf, "workflow.py"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "import subprocess\n"
                    'val = args["target"]\n'
                    'bad = args["undeclared"]\n'
                    'subprocess.run(["git", "push", "origin", "HEAD"])\n'
                )
            safe_findings = skill_linter._lint_customization_paths(
                (safe_wf,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(safe_findings), 1)
            self.assertIn('Argument "undeclared"', safe_findings[0]["message"])

            syn_wf = os.path.join(repo_dir, ".agents", "workflows", "syn-err")
            os.makedirs(syn_wf)
            with open(
                os.path.join(syn_wf, "workflow.json"), "w", encoding="utf-8"
            ) as f:
                json.dump(
                    {"name": "syn-err", "description": "Syntax check."}, f
                )
            with open(
                os.path.join(syn_wf, "workflow.py"), "w", encoding="utf-8"
            ) as f:
                f.write("def broken(:\n")
            syn_findings = skill_linter._lint_customization_paths(
                (syn_wf,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(syn_findings), 1)
            self.assertEqual(syn_findings[0]["level"], "error")
            self.assertIn("Python syntax error", syn_findings[0]["message"])

    def test_unreadable_skill_and_readme_report_errors(self) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            skill_dir = os.path.join(
                repo_dir, ".agents", "skills", "unreadable"
            )
            os.makedirs(skill_dir)
            skill_md = os.path.join(skill_dir, "SKILL.md")
            with open(skill_md, "w", encoding="utf-8") as f:
                f.write("---\nname: unreadable\ndescription: Test.\n---\n")

            wf_dir = os.path.join(repo_dir, ".agents", "workflows", "wf-readme")
            os.makedirs(wf_dir)
            with open(
                os.path.join(wf_dir, "workflow.json"), "w", encoding="utf-8"
            ) as f:
                json.dump(
                    {
                        "name": "wf-readme",
                        "description": "Reads readme.",
                        "library": True,
                    },
                    f,
                )
            readme_md = os.path.join(wf_dir, "README.md")
            with open(readme_md, "w", encoding="utf-8") as f:
                f.write("# Readme\n")

            real_open = open

            def fail_open(file: Any, *args: Any, **kwargs: Any) -> Any:
                if os.path.abspath(str(file)) in (
                    os.path.abspath(skill_md),
                    os.path.abspath(readme_md),
                ):
                    raise OSError("permission denied")
                return real_open(file, *args, **kwargs)

            with mock.patch.object(
                skill_linter,
                "open",
                new=mock.create_autospec(
                    open, spec_set=True, side_effect=fail_open
                ),
                create=True,
            ):
                skill_findings = skill_linter._lint_customization_paths(
                    (skill_md,), repo_root=repo_dir, fuchsia_root=repo_dir
                )
                readme_findings = skill_linter._lint_customization_paths(
                    (wf_dir,), repo_root=repo_dir, fuchsia_root=repo_dir
                )

            self.assertEqual(len(skill_findings), 1)
            self.assertEqual(skill_findings[0]["level"], "error")
            self.assertIn(
                "Could not read SKILL.md", skill_findings[0]["message"]
            )
            self.assertEqual(len(readme_findings), 1)
            self.assertEqual(readme_findings[0]["level"], "error")
            self.assertIn(
                "Could not read workflow README.md",
                readme_findings[0]["message"],
            )

    def test_main_cli_lint_json_and_strict_on_non_skill_files(self) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            wf_dir = os.path.join(repo_dir, ".agents", "workflows", "cli-demo")
            os.makedirs(wf_dir)
            manifest = os.path.join(wf_dir, "workflow.json")
            script = os.path.join(wf_dir, "workflow.py")
            readme = os.path.join(wf_dir, "README.md")
            skills_json = os.path.join(repo_dir, ".agents", "skills.json")
            skill_dir = os.path.join(repo_dir, ".agents", "skills", "cli-skill")
            os.makedirs(skill_dir)
            with open(
                os.path.join(skill_dir, "SKILL.md"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "---\n"
                    "name: cli-skill\n"
                    "description: CLI test. Use when testing. Don't use for"
                    " prod.\n"
                    "---\n\n"
                    "# CLI\n"
                )
            with open(manifest, "w", encoding="utf-8") as f:
                json.dump(
                    {"name": "cli-demo", "description": "Valid workflow."}, f
                )
            with open(script, "w", encoding="utf-8") as f:
                f.write("pass\n")
            with open(readme, "w", encoding="utf-8") as f:
                f.write("# Demo\n")
            with open(skills_json, "w", encoding="utf-8") as f:
                json.dump(
                    {"entries": [{"path": ".agents/skills/cli-skill"}]}, f
                )

            stdout = io.StringIO()
            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "skill_linter.py",
                        "--lint-json",
                        "--repo-root",
                        repo_dir,
                        "--fuchsia-root",
                        repo_dir,
                        manifest,
                        skills_json,
                    ],
                ),
                mock.patch.object(sys, "stdout", stdout),
                self.assertRaises(SystemExit) as cm_json,
            ):
                skill_linter.main()
            self.assertEqual(cm_json.exception.code, 0)
            self.assertSequenceEqual(json.loads(stdout.getvalue()), ())

            with (
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "skill_linter.py",
                        "--strict",
                        "--repo-root",
                        repo_dir,
                        "--fuchsia-root",
                        repo_dir,
                        manifest,
                        script,
                        readme,
                        skills_json,
                    ],
                ),
                self.assertRaises(SystemExit) as cm_strict,
            ):
                skill_linter.main()
            self.assertEqual(cm_strict.exception.code, 0)

    def test_unfixable_name_reports_error_under_fixit(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            for raw_name in ("___", '"@@@"'):
                skill_path = os.path.join(temp_dir, "SKILL.md")
                with open(skill_path, "w", encoding="utf-8") as f:
                    f.write(
                        "---\n"
                        f"name: {raw_name}\n"
                        "description: Valid description.\n"
                        "---\n\n"
                        "# Title\n"
                    )
                code_fix, _ = skill_linter.lint_single_skill(
                    skill_file=skill_path,
                    fixit=True,
                    stdout_mode=False,
                    suggest_fix_in_json_mode=False,
                )
                self.assertEqual(code_fix, 1)
                with open(skill_path, "r", encoding="utf-8") as f:
                    after_fix = f.read()
                meta, _ = skill_linter._parse_skill_frontmatter(after_fix)
                self.assertIsInstance(meta["name"], str)

                _, findings = skill_linter.lint_single_skill(
                    skill_file=skill_path,
                    fixit=False,
                    stdout_mode=False,
                    suggest_fix_in_json_mode=True,
                )
                self.assertEqual(len(findings), 1)
                self.assertEqual(findings[0]["level"], "error")
                self.assertIn(
                    "lowercase letters, numbers, and single hyphens",
                    findings[0]["message"],
                )

    def test_lint_customization_paths_descends_into_container_dirs(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            skills_root = os.path.join(repo_dir, ".agents", "skills")
            bad_skill_dir = os.path.join(skills_root, "bad-demo")
            os.makedirs(bad_skill_dir)
            with open(
                os.path.join(bad_skill_dir, "SKILL.md"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "---\n"
                    "name: bad-demo\n"
                    "description: Short.\n"
                    "---\n\n"
                    "[broken](references/missing.md)\n"
                )

            wf_root = os.path.join(repo_dir, ".agents", "workflows")
            bad_wf_dir = os.path.join(wf_root, "bad-wf")
            os.makedirs(bad_wf_dir)
            with open(
                os.path.join(bad_wf_dir, "workflow.json"),
                "w",
                encoding="utf-8",
            ) as f:
                json.dump({"name": "bad-wf", "description": ""}, f)

            skill_findings = skill_linter._lint_customization_paths(
                (skills_root,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertTrue(
                any(
                    "Broken local link" in fd["message"]
                    for fd in skill_findings
                )
            )
            wf_findings = skill_linter._lint_customization_paths(
                (wf_root,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertTrue(
                any(
                    '"description" cannot be empty' in fd["message"]
                    for fd in wf_findings
                )
            )

    def test_nested_subsystem_suite_folder_naming(self) -> None:
        with tempfile.TemporaryDirectory() as repo_dir:
            clean_nested = os.path.join(
                repo_dir,
                "src",
                "devices",
                "skills",
                "driver-interrupt",
                "implementation",
                "cpp",
            )
            os.makedirs(clean_nested)
            clean_md = os.path.join(clean_nested, "SKILL.md")
            with open(clean_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: driver-interrupt-impl-cpp\n"
                    "description: >-\n"
                    "  Implements C++ driver interrupts. Use when writing C++"
                    " interrupt handlers. Don't use for Rust drivers.\n"
                    "---\n\n"
                    "# C++ driver interrupts\n"
                )
            self.assertSequenceEqual(
                skill_linter._lint_customization_paths(
                    (clean_md,), repo_root=repo_dir, fuchsia_root=repo_dir
                ),
                (),
            )

            underscore_nested = os.path.join(
                repo_dir,
                "src",
                "devices",
                "skills",
                "driver_fidl",
                "server",
                "implementation",
                "rust",
            )
            os.makedirs(underscore_nested)
            underscore_md = os.path.join(underscore_nested, "SKILL.md")
            with open(underscore_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: driver-fidl-server-impl-rust\n"
                    "description: >-\n"
                    "  Implements Rust FIDL servers. Use when writing Rust FIDL"
                    " servers. Don't use for C++ drivers.\n"
                    "---\n\n"
                    "# Rust FIDL servers\n"
                )
            findings = skill_linter._lint_customization_paths(
                (underscore_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["level"], "warning")
            self.assertIn(
                'Nested skill folder "driver_fidl" uses underscores',
                findings[0]["message"],
            )

            leaf_matches_parent_underscore = os.path.join(
                repo_dir,
                "src",
                "devices",
                "skills",
                "my_topic",
                "my-task",
            )
            os.makedirs(leaf_matches_parent_underscore)
            leaf_md = os.path.join(leaf_matches_parent_underscore, "SKILL.md")
            with open(leaf_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: my-task\n"
                    "description: >-\n"
                    "  Runs my task. Use when running my task. Don't use for"
                    " other tasks.\n"
                    "---\n\n"
                    "# My task\n"
                )
            leaf_findings = skill_linter._lint_customization_paths(
                (leaf_md,), repo_root=repo_dir, fuchsia_root=repo_dir
            )
            self.assertEqual(len(leaf_findings), 1)
            self.assertEqual(leaf_findings[0]["level"], "warning")
            self.assertIn(
                'Nested skill folder "my_topic" uses underscores',
                leaf_findings[0]["message"],
            )

    def test_four_backtick_fences_composed_var_cmds_and_dot_agents_dual_path(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as fuchsia_dir:
            # 1. 4-backtick outer fence wrapping an odd number of 3-backtick
            # inner lines does not invert `in_code` or flag template links.
            skill_dir = os.path.join(
                fuchsia_dir, ".agents", "skills", "fence-demo"
            )
            os.makedirs(skill_dir)
            skill_md = os.path.join(skill_dir, "SKILL.md")
            with open(skill_md, "w", encoding="utf-8") as f:
                f.write(
                    "---\n"
                    "name: fence-demo\n"
                    "description: >-\n"
                    "  Tests nested fences. Use when testing fences. Don't use"
                    " for builds.\n"
                    "---\n\n"
                    "````markdown\n"
                    "Example template with [fake](nonexistent.md):\n"
                    "```bash\n"
                    "echo hi\n"
                    "````\n\n"
                    "Prose after 4-backtick block: [missing](bad.md)\n"
                )
            fence_findings = skill_linter._lint_customization_paths(
                (skill_md,), repo_root=fuchsia_dir, fuchsia_root=fuchsia_dir
            )
            self.assertEqual(len(fence_findings), 1)
            self.assertIn(
                'Broken local link "bad.md"', fence_findings[0]["message"]
            )

            # 2. Reassigned and composed `var_cmds` still detect external mutations.
            wf_dir = os.path.join(
                fuchsia_dir, ".agents", "workflows", "reassigned-cmd"
            )
            os.makedirs(wf_dir)
            with open(
                os.path.join(wf_dir, "workflow.json"), "w", encoding="utf-8"
            ) as f:
                json.dump(
                    {"name": "reassigned-cmd", "description": "Var cmd check."},
                    f,
                )
            with open(
                os.path.join(wf_dir, "workflow.py"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "import subprocess\n"
                    'base = ["git", "push"]\n'
                    'cmd = base + ["origin", "HEAD"]\n'
                    "subprocess.run(cmd)\n"
                    'cmd = ["git", "status"]\n'
                    "subprocess.run(cmd)\n"
                )
            wf_findings = skill_linter._lint_customization_paths(
                (wf_dir,), repo_root=fuchsia_dir, fuchsia_root=fuchsia_dir
            )
            self.assertEqual(len(wf_findings), 1)
            self.assertIn(
                "external mutation commands", wf_findings[0]["message"]
            )

            # 3. `.agents/skills/...` in vendor/google/.agents/skills.json
            # requires a paired `vendor/google/.agents/skills/...` entry, while
            # `../...` traversal paths are skipped.
            vg_dir = os.path.join(fuchsia_dir, "vendor", "google")
            vg_dot_agents = os.path.join(vg_dir, ".agents")
            os.makedirs(os.path.join(vg_dot_agents, "skills", "device-bug-fix"))
            vg_skills_json = os.path.join(vg_dot_agents, "skills.json")
            with open(vg_skills_json, "w", encoding="utf-8") as f:
                json.dump(
                    {
                        "entries": [
                            {"path": "../../.agents/skills/fence-demo"},
                            {"path": ".agents/skills/device-bug-fix"},
                        ]
                    },
                    f,
                )
            vg_findings = skill_linter._lint_customization_paths(
                (vg_skills_json,),
                repo_root=vg_dir,
                fuchsia_root=fuchsia_dir,
            )
            self.assertEqual(len(vg_findings), 1)
            self.assertIn(
                'missing paired "vendor/google/.agents/skills/device-bug-fix"',
                vg_findings[0]["message"],
            )


if __name__ == "__main__":
    unittest.main()
