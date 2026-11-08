# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the check in //scripts/shac/bazel_migration_test_parity.star."""

load(
    "//scripts/shac/bazel_migration_test_parity.star",
    _check = "_bazel_migration_test_parity",
)

_VARS = {"fuchsia_dir": "."}

_TEST_DROP_VERIFIED = {"fail_if_missing_label": "Test-Drop-Verified"}

def _run(*messages, vars = _VARS):
    return testing.run(
        _check,
        commits = [
            testing.commit(hash = str(i) * 40, message = msg)
            for i, msg in enumerate(messages)
        ],
        vars = vars,
    )

def _assert_single_finding(
        res,
        contains,
        level = "error",
        commit_hash = "0" * 40,
        line = None,
        properties = None):
    """Asserts that `res` has one commit-message finding matching the expected fields."""
    asserts.eq(len(res.findings), 1)
    finding = res.findings[0]
    asserts.eq(finding.level, level)
    asserts.eq(finding.commit_hash, commit_hash)
    if line != None:
        asserts.eq(finding.line, line)
    asserts.eq(finding.properties or None, properties)
    for text in contains:
        asserts.contains(finding.message, text)

def test_no_parity_footers():
    res = _run("""\
[multiproduct] Add core.x64-release-minfs tests

Bug: 42012345
Change-Id: I1234567890abcdef1234567890abcdef12345678
""")
    asserts.eq(res.findings, ())

def test_non_fuchsia_repo_ignored():
    res = _run(
        """\
[bazel_migration] //vendor/foo

Deleted-Tests: //vendor/foo:dead_test
""",
        vars = {"fuchsia_dir": "../.."},
    )
    asserts.eq(res.findings, ())

def test_submitted_commit_ignored():
    res = _run("""\
[bazel_migration] //src/foo

Bug: 42012345
Deleted-Tests: //src/foo:dead_test
Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1
""")
    asserts.eq(res.findings, ())

def test_quoted_and_comment_footers_ignored():
    res = _run("""\
Revert "[bazel_migration] //src/foo"

Original change's description:
> Deleted-Tests: //src/foo:dead_test
# Skipped-Dual-Tests: //src/foo:foo_tests

Bug: 42012345
Change-Id: I1234567890abcdef1234567890abcdef12345678
""")
    asserts.eq(res.findings, ())

def test_deleted_tests_requires_test_drop_verified():
    res = _run("""\
[bazel_migration] //src/foo

Fixed: 42012345
Deleted-Tests: //src/foo:dead_test (obsolete test, replaced upstream), //src/foo:other_test
""")
    _assert_single_finding(
        res,
        [
            "ATTESTED ESCAPE-HATCH TEST DELETION (REQUIRES Test-Drop-Verified +1)",
            "//src/foo:dead_test, //src/foo:other_test",
            "(Bug: 42012345)",
        ],
        level = "warning",
        line = 4,
        properties = _TEST_DROP_VERIFIED,
    )

def test_deleted_tests_forbidden_on_planter_commits():
    res = _run("""\
[bazel_migration] //src/foo

Bug: 42012345
Deleted-Tests: //src/foo:dead_test
TAG: planter
""")
    _assert_single_finding(
        res,
        ["FORBIDDEN AGENT Deleted-Tests: FOOTER"],
        line = 4,
    )

def test_deleted_tests_missing_bug():
    res = _run("""\
[bazel_migration] //src/foo

Deleted-Tests: //src/foo:dead_test
""")
    _assert_single_finding(
        res,
        ["missing numeric `Bug: <id>` footer"],
        line = 3,
    )

def test_deleted_tests_wildcard():
    res = _run("""\
[bazel_migration] //src/foo

Bug: 42012345
Deleted-Tests: //src/foo:*
""")
    _assert_single_finding(
        res,
        ["wildcards (`*`) are forbidden"],
        line = 4,
    )

def test_skipped_dual_tests_requires_test_drop_verified():
    res = _run("""\
[bazel_migration] //src/foo

Bug: 42012345
Skipped-Dual-Tests: //src/foo:foo_tests (expensive integration suite)
""")
    _assert_single_finding(
        res,
        [
            "DUAL-BUILD TEST SKIPPED (REQUIRES Test-Drop-Verified +1)",
            "//src/foo:foo_tests",
        ],
        level = "notice",
        line = 4,
        properties = _TEST_DROP_VERIFIED,
    )

def test_skipped_dual_tests_malformed():
    res = _run("""\
[bazel_migration] //src/foo

Skipped-Dual-Tests: not_a_label
""")
    _assert_single_finding(
        res,
        ["INVALID Skipped-Dual-Tests: FOOTER"],
        line = 3,
    )

def test_renamed_tests_one_to_one():
    res = _run("""\
[bazel_migration] //src/foo

Renamed-Tests: //src/foo:old_test -> //src/foo:new_test
""")
    asserts.eq(res.findings, ())

def test_renamed_tests_comma_separated():
    res = _run("""\
[bazel_migration] //src/foo

Renamed-Tests: //src/foo:old1 -> //src/foo:new1, //src/foo:old2 -> //src/foo:new2
""")
    _assert_single_finding(
        res,
        ["INVALID Renamed-Tests: SYNTAX"],
        line = 3,
    )

def test_renamed_tests_collapse():
    res = _run("""\
[bazel_migration] //src/foo

Renamed-Tests: //src/foo:test_a -> //src/foo:merged_test
Renamed-Tests: //src/foo:test_b -> //src/foo:merged_test
""")
    _assert_single_finding(
        res,
        ["NON-BIJECTIVE Renamed-Tests: COLLAPSE"],
        line = 3,
    )

def test_renamed_tests_split():
    res = _run("""\
[bazel_migration] //src/foo

Renamed-Tests: //src/foo:old_test -> //src/foo:new_a
Renamed-Tests: //src/foo:old_test -> //src/foo:new_b
""")
    _assert_single_finding(
        res,
        ["NON-BIJECTIVE Renamed-Tests: SPLIT"],
        line = 3,
    )

def test_each_commit_checked():
    res = _run(
        """\
[bazel_migration] //src/foo

Bug: 42012345
Deleted-Tests: //src/foo:dead_test
""",
        """\
[bazel_migration] //src/bar

Renamed-Tests: //src/bar:old_test -> //src/bar:new_test
""",
        """\
[bazel_migration] //src/baz

Skipped-Dual-Tests: //src/baz:baz_tests
""",
    )
    asserts.eq(
        [(f.level, f.commit_hash, f.line, f.properties or None) for f in res.findings],
        [
            ("warning", "0" * 40, 4, _TEST_DROP_VERIFIED),
            ("error", "2" * 40, 3, None),
        ],
    )
    asserts.contains(res.findings[0].message, "//src/foo:dead_test")
    asserts.contains(res.findings[1].message, "INVALID Skipped-Dual-Tests: FOOTER")
