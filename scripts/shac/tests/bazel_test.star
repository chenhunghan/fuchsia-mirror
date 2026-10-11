# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for the checks in //scripts/shac/bazel.star."""

load(
    "//scripts/shac/bazel.star",
    "_bazel_default_applicable_licenses",
    "_bazel_disallowed_workspace_root_package_labels",
    "_bazel_no_repo_rules_canonical_names",
)

_ALLOWLISTS = testing.file(
    content = json.encode({
        "ignored_files": ["ignored/BUILD.bazel"],
        "ignored_prefixes": ["third_party"],
    }),
    affected = False,
)

_LICENSES_MSG = "BUILD.bazel files must include `default_applicable_licenses = [\"//:license\"]` within a `package()` call to set the default file-level license."

def test_default_applicable_licenses_missing():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "foo/BUILD.bazel": "filegroup(name = \"foo\")\n",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _LICENSES_MSG,
            filepath = "foo/BUILD.bazel",
        ),
    ))

def test_default_applicable_licenses_in_other_call():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "foo/BUILD.bazel": "filegroup(default_applicable_licenses = [\"//:license\"])\n",
    })
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = _LICENSES_MSG,
            filepath = "foo/BUILD.bazel",
        ),
    ))

def test_default_applicable_licenses_present():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "foo/BUILD.bazel": "package(\n    default_applicable_licenses = [\"//:license\"],\n)\n",
    })
    asserts.eq(res.findings, ())

def test_default_applicable_licenses_ignored():
    res = testing.run(_bazel_default_applicable_licenses, files = {
        "build/bazel/shac/allowlists.json": _ALLOWLISTS,
        "ignored/BUILD.bazel": "",
        "third_party/foo/BUILD.bazel": "",
        # Matches the prefix as a string, but not as a directory.
        "third_party_foo/BUILD.bazel": "",
    })
    asserts.eq(
        [f.filepath for f in res.findings],
        ["third_party_foo/BUILD.bazel"],
    )

def test_no_repo_rules_canonical_names():
    res = testing.run(_bazel_no_repo_rules_canonical_names, files = {
        "foo.bzl": "x = 1\ny = \"@@+_repo_rules3+internal_sdk//:foo\"\n",
        "foo.py": "LABEL = \"@@+_repo_rules+internal_sdk//:foo\"\n",
        # Other file types aren't checked.
        "foo.md": "@@+_repo_rules3+internal_sdk\n",
    })
    asserts.eq(
        [(f.filepath, f.level, f.line, f.col, f.end_col) for f in res.findings],
        [("foo.bzl", "error", 2, 9, 22), ("foo.py", "error", 1, 13, 25)],
    )

def test_no_repo_rules_canonical_names_apparent_name():
    res = testing.run(_bazel_no_repo_rules_canonical_names, files = {
        "BUILD.bazel": "x = \"@internal_sdk//:foo\"\n",
    })
    asserts.eq(res.findings, ())

def test_disallowed_workspace_root_package_labels():
    res = testing.run(
        _bazel_disallowed_workspace_root_package_labels,
        files = {
            "foo/BUILD.bazel": "x = \"//:foo/bar\"\n",
            "third_party/foo/src/BUILD.bazel": "",
            "README.md": "",
        },
        exec_mocks = [
            testing.exec_mock(
                cmd = [
                    testing.any_args,
                    "--root",
                    testing.root,
                    "--json",
                    "foo/BUILD.bazel",
                ],
                retcode = 1,
                stdout = json.encode([{
                    "filepath": "foo/BUILD.bazel",
                    "message": "bad label",
                    "line": 1,
                    "col": 6,
                    "end_col": 16,
                }]),
            ),
        ],
    )
    asserts.eq(res.findings, (
        testing.finding(
            level = "error",
            message = "bad label",
            filepath = "foo/BUILD.bazel",
            line = 1,
            col = 6,
            end_col = 16,
        ),
    ))

def test_disallowed_workspace_root_package_labels_inconsistent_retcode():
    asserts.fails(
        lambda: testing.run(
            _bazel_disallowed_workspace_root_package_labels,
            files = {"BUILD.bazel": ""},
            exec_mocks = [
                testing.exec_mock(
                    cmd = [testing.any_args, "--root", testing.root, "--json", "BUILD.bazel"],
                    retcode = 1,
                    stdout = "[]",
                ),
            ],
        ),
        "exited with code 1, but reported 0 findings",
    )
