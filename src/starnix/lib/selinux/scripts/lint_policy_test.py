#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for `lint_policy.py`."""

import contextlib
import io
import json
import pathlib
import tempfile
import unittest
from collections.abc import Sequence

import lint_policy

_DEMO_PATH = "tests/selinux/userspace/policies/demo_policy.conf"
_BASE_SYMBOLS = frozenset(
    {"domain_a", "file_like_a", "file_t", "test_a", "tmpfs_t", "unconfined_t"}
)


def _lint(content: str) -> Sequence[tuple[int, str]]:
    return tuple(
        (f.line, f.rule)
        for f in lint_policy.lint_policy_text(
            _DEMO_PATH, content, _BASE_SYMBOLS
        )
    )


class LintPolicyTest(unittest.TestCase):
    def test_clean_positive_and_negative_policy(self):
        policy = """\
# Commented lines and inline conditionals are handled cleanly.
attribute test_demo_a;
type test_demo_t, test_a, test_demo_a;
type test_demo_no_write_t, test_a;
type test_demo_no_trans_t, test_a;
type test_demo_file_t, file_like_a;
role test_demo_r1;
user test_demo_u1 roles { test_demo_r1 } level s0 range s0 - s0;
allow test_demo_t test_demo_file_t:file *;
allow test_demo_no_write_t test_demo_file_t:file ~{ write };
neverallow test_demo_no_write_t test_demo_file_t:file { write };
allow test_demo_t self:capability { net_admin };
if (test_demo_bool) { allow test_demo_t test_demo_file_t:file { read }; }
"""
        self.assertSequenceEqual(_lint(policy), ())

    def test_rules_table(self):
        cases = (
            (
                "naming_prefix_and_suffix",
                "attribute bad_attr;\n"
                "type wrong_s, test_a;\n"
                "role test_demo_role;\n"
                "user bad_user roles { unconfined_r } level s0 range s0 - s0;\n",
                (
                    (1, "naming_prefix"),
                    (1, "naming_suffix"),
                    (2, "naming_prefix"),
                    (2, "naming_suffix"),
                    (3, "naming_suffix"),
                    (4, "naming_prefix"),
                    (4, "naming_suffix"),
                ),
            ),
            (
                "no_domain_a_on_test_domain_inline_and_separate",
                "type test_demo_a_t, test_a, domain_a;\n"
                "type test_demo_b_t, test_a;\n"
                "typeattribute test_demo_b_t domain_a;\n"
                "type test_demo_c_t, domain_a;\n"
                "typeattribute test_demo_c_t test_a;\n",
                (
                    (1, "no_domain_a_on_test_domain"),
                    (3, "no_domain_a_on_test_domain"),
                    (5, "no_domain_a_on_test_domain"),
                ),
            ),
            (
                "capability_target_self",
                "type test_demo_t, test_a;\n"
                "allow test_demo_t self:capability { net_admin };\n"
                "allow test_demo_t test_demo_t:capability2 { mac_admin };\n"
                "neverallow test_demo_t test_demo_t:cap_userns { sys_admin };\n",
                (
                    (3, "capability_target_self"),
                    (4, "capability_target_self"),
                ),
            ),
            (
                "wildcard_and_negation_guardrails",
                "type test_demo_t, test_a;\n"
                "type test_demo_file_t, file_like_a;\n"
                "allow test_demo_t self:capability *;\n"
                "allow test_demo_t self:process *;\n"
                "allow test_a test_demo_file_t:file *;\n"
                "allow test_demo_t file_t:file *;\n"
                "allow test_demo_t *:file *;\n"
                "allow test_demo_t ~test_demo_file_t:file *;\n"
                "allow test_demo_t self:* *;\n"
                "allow test_demo_t ~self:capability { net_admin };\n"
                "allow ~unconfined_t test_demo_file_t:file { read };\n",
                (
                    (3, "no_wildcard_capability"),
                    (4, "no_wildcard_process_or_base_target"),
                    (5, "no_wildcard_process_or_base_target"),
                    (6, "no_wildcard_process_or_base_target"),
                    (7, "no_wildcard_process_or_base_target"),
                    (8, "no_wildcard_process_or_base_target"),
                    (9, "no_wildcard_capability"),
                    (9, "no_wildcard_process_or_base_target"),
                    (10, "capability_target_self"),
                    (10, "no_wildcard_process_or_base_target"),
                    (11, "no_wildcard_process_or_base_target"),
                ),
            ),
            (
                "inline_if_with_braced_rule_and_parens",
                "type test_demo_t, test_a;\n"
                "type test_demo_file_t, file_like_a;\n"
                "if (a && (b || c)) { allow test_demo_t test_demo_file_t:file ~ { write }; }\n",
                ((3, "paired_neverallow_for_negation"),),
            ),
            (
                "paired_neverallow_self_and_spaced_negation",
                "attribute test_demo_a;\n"
                "type test_demo_t, test_a, test_demo_a;\n"
                "type test_demo_file_t, file_like_a;\n"
                "allow test_demo_t self:file ~ { write };\n"
                "neverallow test_demo_t test_demo_t:file { write };\n"
                "allow test_demo_t test_demo_t:dir ~{ setattr };\n"
                "neverallow test_demo_t self:dir { setattr };\n"
                "allow test_demo_a test_demo_a:file ~{ unlink };\n"
                "neverallow test_demo_a self:file { unlink };\n",
                ((8, "paired_neverallow_for_negation"),),
            ),
            (
                "paired_neverallow_for_negation_missing_and_attr_covered",
                "attribute test_demo_neg_a;\n"
                "type test_demo_t, test_a, test_demo_neg_a;\n"
                "type test_demo_file_t, file_like_a;\n"
                "allow test_demo_t test_demo_file_t:file ~{ read write };\n"
                "neverallow test_demo_neg_a test_demo_file_t:file { read };\n",
                ((4, "paired_neverallow_for_negation"),),
            ),
            (
                "negative_domain_requires_neverallow",
                "attribute test_demo_covered_a;\n"
                "type test_demo_without_read_t, test_a;\n"
                "type test_demo_deny_write_t, test_a, test_demo_covered_a;\n"
                "type test_demo_no_trans_t, test_a;\n"
                "neverallow test_demo_covered_a self:file { write };\n",
                ((2, "negative_domain_requires_neverallow"),),
            ),
        )
        for name, content, expected in cases:
            with self.subTest(name=name):
                self.assertSequenceEqual(_lint(content), expected)

    def test_main_loads_base_symbols_and_emits_json(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            for rel, text in (
                (
                    "lib/selinux/testdata/policy/base/filesystems.conf",
                    "type tmpfs_t;\n",
                ),
                (
                    "lib/selinux/testdata/policy/base/minimal_base.conf",
                    "attribute domain_a;\ntype file_t;\n",
                ),
                (
                    "tests/selinux/userspace/policies/test_policy.conf",
                    "attribute test_a;\n",
                ),
                (
                    _DEMO_PATH,
                    "type test_demo_t, test_a;\nallow test_demo_t tmpfs_t:file *;\n",
                ),
            ):
                path = root / rel
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")

            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = lint_policy.main(["--root", str(root), _DEMO_PATH])

        self.assertEqual(code, 0)
        decoded = json.loads(out.getvalue())
        self.assertEqual(len(decoded), 1)
        self.assertEqual(
            decoded[0]["rule"], "no_wildcard_process_or_base_target"
        )
        self.assertEqual(decoded[0]["line"], 2)


if __name__ == "__main__":
    unittest.main()
