#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for commit_message_format.sh."""

import json
import os
import subprocess
import tempfile
import unittest

SCRIPT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "commit_message_format.sh"
)


def _git(repo: str, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", repo, *args], text=True
    ).strip()


def _init_repo(tmpdir: str, files: dict[str, str], message: str) -> str:
    _git(tmpdir, "init", "-q")
    _git(tmpdir, "config", "user.email", "test@example.com")
    _git(tmpdir, "config", "user.name", "Test User")
    for rel_path, content in files.items():
        full_path = os.path.join(tmpdir, rel_path)
        os.makedirs(os.path.dirname(full_path), exist_ok=True)
        with open(full_path, "w", encoding="utf-8") as f:
            f.write(content)
    _git(tmpdir, "add", ".")
    _git(tmpdir, "commit", "-q", "-m", "base")
    base_sha = _git(tmpdir, "rev-parse", "HEAD")
    return base_sha


def _run_commit_message_format(
    repo: str, base_sha: str, changed_files: list[str]
) -> list[dict]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")}
    env.update(
        {
            "PLANTER_WORKDIR": repo,
            "PLANTER_CHANGE_BASE": base_sha,
            "PLANTER_RESOLVED_CHANGE_BASE": base_sha,
            "PLANTER_CHANGED_FILES": "\n".join(changed_files),
        }
    )
    git_dir = _git(repo, "rev-parse", "--absolute-git-dir")
    with open(os.path.join(git_dir, "FETCH_HEAD"), "w") as fh:
        fh.write(base_sha + "\n")
    out = subprocess.run(
        ["bash", SCRIPT], env=env, capture_output=True, text=True, check=True
    )
    return json.loads(out.stdout)


class CommitMessageFormatTest(unittest.TestCase):
    def test_source_deletion_wording(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/foo/BUILD.gn": "",
                    "src/foo/lib.cc": "int foo() { return 0; }",
                },
                "base",
            )

            # Delete lib.cc and write new commit message
            _git(tmpdir, "rm", "src/foo/lib.cc")
            msg = """[bazel_migration] //src/foo

Delete lib.cc because it is unused dead code.

Test: fx lint
Change-Id: I1234567890
"""
            _git(tmpdir, "commit", "-q", "-m", msg)

            findings = _run_commit_message_format(
                tmpdir, base, ["src/foo/lib.cc"]
            )
            cats = [
                (f["category"], f.get("message", ""))
                for f in findings
                if f["category"] == "commit_body_unmentioned_source_fix"
            ]
            self.assertEqual(len(cats), 0, cats)

    def test_converted_rustc_staticlib_and_companion_headers_bmt(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/wlan/BUILD.gn": (
                        'rustc_staticlib("wlansoftmac-c") {\n'
                        '  sources = ["src/lib.rs"]\n'
                        '  public = ["c.h"]\n'
                        "}\n"
                    ),
                    "src/wlan/BUILD.bazel": (
                        'load("@rules_rust//rust:defs.bzl", "rust_static_library")\n\n'
                        "rust_static_library(\n"
                        '    name = "wlansoftmac-c",\n'
                        '    srcs = ["src/lib.rs"],\n'
                        ")\n"
                    ),
                },
                "base",
            )
            with open(
                os.path.join(tmpdir, "src/wlan/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n'
                    'load("//build/bazel/rules/rust:rustc_staticlib.bzl", "rustc_staticlib")\n\n'
                    "rustc_staticlib(\n"
                    '    name = "wlansoftmac-c",\n'
                    '    srcs = ["src/lib.rs"],\n'
                    ")\n\n"
                    "fx_cc_library(\n"
                    '    name = "wlansoftmac-c_headers",\n'
                    '    hdrs = ["c.h"],\n'
                    '    deps = [":wlansoftmac-c"],\n'
                    ")\n"
                )
            with open(
                os.path.join(tmpdir, "src/wlan/BUILD.gn"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n\n"
                    'rustc_staticlib("wlansoftmac-c") {\n'
                    '  sources = ["src/lib.rs"]\n'
                    "}\n"
                )
            _git(tmpdir, "add", "src/wlan/BUILD.bazel", "src/wlan/BUILD.gn")
            msg = """[bazel_migration] //src/wlan

Convert wlansoftmac-c to rustc_staticlib synced via bazel2gn and add
the companion wlansoftmac-c_headers target for C++ consumers.

Test: fx bazel build --config=fuchsia_platform @//src/wlan:wlansoftmac-c
Test: fx lint
Bazel-Migration-Target: //src/wlan:wlansoftmac-c
Change-Id: I1234567890
"""
            _git(tmpdir, "commit", "-q", "-m", msg)
            findings = _run_commit_message_format(
                tmpdir, base, ["src/wlan/BUILD.bazel", "src/wlan/BUILD.gn"]
            )
            bmt_errors = [
                f
                for f in findings
                if f["category"]
                in (
                    "bazel_migration_target_not_migrated",
                    "missing_bazel_migration_target_footer",
                )
            ]
            self.assertEqual(bmt_errors, [], bmt_errors)

    def test_dead_code_build_gn_deletion_rejects_migrate_body(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/dead/BUILD.gn": 'group("dead") {}\n',
                },
                "base",
            )
            _git(tmpdir, "rm", "src/dead/BUILD.gn")
            bad_msg = """[bazel_migration] //src/dead

Migrate //src/dead from GN to Bazel.

Test: fx lint
Change-Id: I1234567890
"""
            _git(tmpdir, "commit", "-q", "-m", bad_msg)
            findings = _run_commit_message_format(
                tmpdir, base, ["src/dead/BUILD.gn"]
            )
            errs = [
                f
                for f in findings
                if f["category"] == "commit_body_claims_unmigrated_target"
                and f["severity"] == "error"
            ]
            self.assertEqual(len(errs), 1, findings)

            good_msg = """[bazel_migration] //src/dead

Delete unused src/dead/BUILD.gn because the package has no remaining GN
consumers and nothing is migrated to Bazel.

Test: fx lint
Change-Id: I1234567890
"""
            _git(tmpdir, "commit", "-q", "--amend", "-m", good_msg)
            findings2 = _run_commit_message_format(
                tmpdir, base, ["src/dead/BUILD.gn"]
            )
            errs2 = [
                f
                for f in findings2
                if f["category"] == "commit_body_claims_unmigrated_target"
            ]
            self.assertEqual(errs2, [], findings2)

    def test_plus_sign_in_target_labels(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "sdk/lib/stdcompat/test/BUILD.gn": 'config("stdcompat-c++-17") {}\n',
                },
                "base",
            )
            with open(
                os.path.join(tmpdir, "sdk/lib/stdcompat/test/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    'load("//build/bazel/rules:build_flags.bzl", "build_flags")\n\n'
                    'build_flags(name = "stdcompat-c++-17")\n'
                )
            with open(
                os.path.join(tmpdir, "sdk/lib/stdcompat/test/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n\n"
                    'config("stdcompat-c++-17") {}\n'
                )
            _git(
                tmpdir,
                "add",
                "sdk/lib/stdcompat/test/BUILD.bazel",
                "sdk/lib/stdcompat/test/BUILD.gn",
            )
            msg = """[bazel_migration][stdcompat] //sdk/lib/stdcompat/test

Migrate stdcompat-c++-17 config to BUILD.bazel and sync BUILD.gn via
bazel2gn.

Test: fx bazel build --config=fuchsia_platform @//sdk/lib/stdcompat/test:stdcompat-c++-17
Test: fx lint
Bazel-Migration-Target: //sdk/lib/stdcompat/test:stdcompat-c++-17
Change-Id: I1234567890
"""
            _git(tmpdir, "commit", "-q", "-m", msg)
            findings = _run_commit_message_format(
                tmpdir,
                base,
                [
                    "sdk/lib/stdcompat/test/BUILD.bazel",
                    "sdk/lib/stdcompat/test/BUILD.gn",
                ],
            )
            bad = [
                f
                for f in findings
                if f["category"] == "bazel_migration_target_footer_format"
            ]
            self.assertEqual(bad, [], findings)

    def test_canonical_verify_bazel2gn_footer_without_fx_lint(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/foo/BUILD.gn": 'source_set("foo") {}\n',
                },
                "base",
            )
            with open(
                os.path.join(tmpdir, "src/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                    'fx_cc_library(name = "foo", srcs = ["a.cc"])\n'
                )
            with open(
                os.path.join(tmpdir, "src/foo/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n\n"
                    'source_set("foo") {}\n'
                )
            _git(tmpdir, "add", "src/foo/BUILD.bazel", "src/foo/BUILD.gn")
            msg = """[bazel_migration] //src/foo

Migrate foo to BUILD.bazel and sync BUILD.gn via bazel2gn.

Test: fx bazel build --config=fuchsia_platform @//src/foo
Test: fx host-tool shac check --only verify_bazel2gn
Bazel-Migration-Target: //src/foo:foo
Change-Id: I1234567890
"""
            _git(tmpdir, "commit", "-q", "-m", msg)
            findings = _run_commit_message_format(
                tmpdir, base, ["src/foo/BUILD.bazel", "src/foo/BUILD.gn"]
            )
            bad = [
                f
                for f in findings
                if f["category"] == "test_footer_bazel2gn_form"
            ]
            self.assertEqual(bad, [], findings)

            bad_msg = """[bazel_migration] //src/foo

Migrate foo to BUILD.bazel and sync BUILD.gn via bazel2gn.

Test: fx bazel build --config=fuchsia_platform @//src/foo
Test: fx host-tool shac check
Bazel-Migration-Target: //src/foo:foo
Change-Id: I1234567890
"""
            _git(tmpdir, "commit", "-q", "--amend", "-m", bad_msg)
            findings2 = _run_commit_message_format(
                tmpdir, base, ["src/foo/BUILD.bazel", "src/foo/BUILD.gn"]
            )
            bad2 = [
                f
                for f in findings2
                if f["category"] == "test_footer_bazel2gn_form"
            ]
            self.assertEqual(len(bad2), 1, findings2)


if __name__ == "__main__":
    unittest.main()
