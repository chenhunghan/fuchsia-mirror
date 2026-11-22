# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for //src/starnix/lib/selinux/scripts/selinux_policy.star."""

load("//src/starnix/lib/selinux/scripts/selinux_policy.star", "selinux_policy_lint")

def test_ignores_non_suite_policy_files():
    res = testing.run(
        selinux_policy_lint,
        subdir = "src/starnix",
        files = {
            "kernel/main.rs": "fn main() {}\n",
            "lib/selinux/testdata/composite_policies/new_file/minimal_policy.conf": "type a_t;\n",
            "tests/selinux/userspace/policies/test_policy.conf": "attribute test_a;\n",
            "tests/selinux/userspace/policies/sub/nested_policy.conf": "type a_t;\n",
        },
    )
    asserts.eq(res.findings, ())

def test_changed_only_vs_whole_file_filtering():
    policy = "tests/selinux/userspace/policies/demo_policy.conf"
    findings = [
        # Legacy-debt rule on untouched line 1 -> suppressed.
        {"rule": "naming_prefix", "filepath": policy, "line": 1, "message": "legacy"},
        # Legacy-debt rule on changed line 2 -> emitted.
        {"rule": "naming_suffix", "filepath": policy, "line": 2, "message": "new bad suffix"},
        # Zero-debt rule on untouched line 3 -> still emitted whole-file.
        {"rule": "paired_neverallow_for_negation", "filepath": policy, "line": 3, "message": "missing neverallow"},
    ]
    res = testing.run(
        selinux_policy_lint,
        subdir = "src/starnix",
        files = {
            policy: testing.file(
                content = "type bad_t, test_a;\ntype test_demo_s, test_a;\nallow test_demo_s test_demo_f_t:file ~{ write };\n",
                affected_lines = [(2, "type test_demo_s, test_a;")],
            ),
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    testing.root + "/lib/selinux/scripts/lint_policy.py",
                    "--root",
                    testing.root,
                    policy,
                ],
                stdout = json.encode(findings),
            ),
        ],
    )
    asserts.eq(
        res.findings,
        (
            testing.finding(
                level = "error",
                filepath = policy,
                line = 2,
                message = "[naming_suffix] new bad suffix",
            ),
            testing.finding(
                level = "error",
                filepath = policy,
                line = 3,
                message = "[paired_neverallow_for_negation] missing neverallow",
            ),
        ),
    )
