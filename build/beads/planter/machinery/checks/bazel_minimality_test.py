#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for redundant default lint_config detection in bazel_minimality.sh."""

import json
import os
import subprocess
import tempfile
import unittest

SCRIPT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "bazel_minimality.sh"
)

HEADER = """\
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""


def _git(repo: str, *args: str) -> str:
    return subprocess.check_output(
        ["git", "-C", repo, *args], text=True
    ).strip()


def _init_repo(tmpdir: str, files: dict[str, str]) -> str:
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
    return _git(tmpdir, "rev-parse", "HEAD")


def _run_minimality(
    repo: str, base_sha: str, target_dir: str, changed_files: list[str]
) -> list[dict]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")}
    env.update(
        {
            "PLANTER_WORKDIR": repo,
            "PLANTER_TARGET_DIR": target_dir,
            "PLANTER_TARGET_DIRS": target_dir,
            "PLANTER_CHANGE_BASE": base_sha,
            "PLANTER_RESOLVED_CHANGE_BASE": base_sha,
            "PLANTER_CHANGED_FILES": "\n".join(changed_files),
        }
    )
    out = subprocess.run(
        ["bash", SCRIPT], env=env, capture_output=True, text=True, check=True
    )
    return json.loads(out.stdout)


class BazelMinimalityLintConfigTest(unittest.TestCase):
    def test_testonly_rustc_library_redundant_and_overrides(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            rel = "src/foo/BUILD.bazel"
            full = os.path.join(tmpdir, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "bad_testonly",\n'
                    + "    testonly = True,\n"
                    + '    srcs = ["src/a.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + ")\n\n"
                    + "rustc_library(\n"
                    + '    name = "bad_testonly_with_unit_tests",\n'
                    + "    testonly = True,\n"
                    + '    srcs = ["src/a2.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + "    with_unit_tests = True,\n"
                    + ")\n\n"
                    + "rustc_library(\n"
                    + '    name = "ok_testonly_prod",\n'
                    + "    testonly = True,\n"
                    + '    srcs = ["src/b.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_production",\n'
                    + ")\n\n"
                    + "rustc_library(\n"
                    + '    name = "ok_testonly_warn_all",\n'
                    + "    testonly = True,\n"
                    + '    srcs = ["src/c.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_all",\n'
                    + ")\n"
                )
            findings = _run_minimality(tmpdir, base, "src/foo", [rel])
            self.assertEqual(len(findings), 2, findings)
            self.assertIn("bad_testonly", findings[0]["message"])
            self.assertIn(
                "bad_testonly_with_unit_tests", findings[1]["message"]
            )
            for finding in findings:
                self.assertEqual(finding["severity"], "error")
                self.assertTrue(
                    finding["message"].endswith("when `testonly = True`.")
                )

    def test_rustc_test_and_host_rustc_test_redundant(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            rel = "src/foo/BUILD.bazel"
            full = os.path.join(tmpdir, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n'
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library", "rustc_test")\n\n'
                    + "rustc_library(\n"
                    + '    name = "foo",\n'
                    + '    srcs = ["src/other.rs"],\n'
                    + '    edition = "2024",\n'
                    + ")\n\n"
                    + "rustc_test(\n"
                    + '    name = "foo_test",\n'
                    + '    crate = ":foo",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + ")\n\n"
                    + "host_rustc_test(\n"
                    + '    name = "foo_host_test",\n'
                    + '    srcs = ["tests/host.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + ")\n"
                )
            findings = _run_minimality(tmpdir, base, "src/foo", [rel])
            self.assertEqual(len(findings), 2, findings)
            for finding in findings:
                self.assertEqual(finding["severity"], "error")
                self.assertFalse(
                    finding["message"].endswith("when `testonly = True`.")
                )
                self.assertTrue(finding["message"].endswith("by default."))

    def test_production_default_and_label_normalization(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            rel = "src/foo/BUILD.bazel"
            full = os.path.join(tmpdir, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/idk:idk_host_tool.bzl", "idk_rustc_binary_host_tool")\n'
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "prod_lib",\n'
                    + '    srcs = ["src/other.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "@//build/config/rust/lints:clippy_warn_production",\n'
                    + ")\n\n"
                    + "idk_rustc_binary_host_tool(\n"
                    + '    name = "prod_idk_tool",\n'
                    + '    srcs = ["src/tool.rs"],\n'
                    + '    api_area = "Developer",\n'
                    + '    category = "partner",\n'
                    + '    edition = "2024",\n'
                    + '    idk_name = "prod_idk_tool",\n'
                    + '    lint_config = "@@//build/config/rust/lints:clippy_warn_production",\n'
                    + ")\n"
                )
            lints_rel = "build/config/rust/lints/BUILD.bazel"
            lints_full = os.path.join(tmpdir, lints_rel)
            os.makedirs(os.path.dirname(lints_full), exist_ok=True)
            with open(lints_full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "local_testonly",\n'
                    + "    testonly = True,\n"
                    + '    srcs = ["src/local.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = ":clippy_warn_default",\n'
                    + ")\n"
                )
            findings = _run_minimality(
                tmpdir,
                base,
                "src/foo,build/config/rust/lints",
                [rel, lints_rel],
            )
            names = [f["message"] for f in findings]
            self.assertEqual(len(findings), 3, findings)
            self.assertTrue(any("prod_lib" in m for m in names))
            self.assertTrue(any("prod_idk_tool" in m for m in names))
            self.assertTrue(any("local_testonly" in m for m in names))

    def test_inline_unit_tests_exempt_from_production_default(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            rel = "src/foo/BUILD.bazel"
            full = os.path.join(tmpdir, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/idk:idk_host_tool.bzl", "idk_rustc_binary_host_tool")\n'
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "lib_with_tests",\n'
                    + '    srcs = ["src/other.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_production",\n'
                    + "    with_unit_tests = True,\n"
                    + ")\n\n"
                    + "idk_rustc_binary_host_tool(\n"
                    + '    name = "tool_with_host_tests",\n'
                    + '    srcs = ["src/tool.rs"],\n'
                    + '    api_area = "Developer",\n'
                    + '    category = "partner",\n'
                    + '    edition = "2024",\n'
                    + '    idk_name = "tool_with_host_tests",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_production",\n'
                    + "    with_host_unit_tests = True,\n"
                    + ")\n"
                )
            findings = _run_minimality(tmpdir, base, "src/foo", [rel])
            self.assertEqual(findings, [])

    def test_ffx_plugin_defaults_to_production_only(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            rel = "src/foo/BUILD.bazel"
            full = os.path.join(tmpdir, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//src/developer/ffx/build:ffx_plugin.bzl", "ffx_plugin")\n\n'
                    + "ffx_plugin(\n"
                    + '    name = "plugin_redundant_prod",\n'
                    + '    args_srcs = ["src/args.rs"],\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_production",\n'
                    + ")\n\n"
                    + "ffx_plugin(\n"
                    + '    name = "plugin_explicit_default",\n'
                    + "    testonly = True,\n"
                    + '    args_srcs = ["src/args2.rs"],\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + ")\n"
                )
            findings = _run_minimality(tmpdir, base, "src/foo", [rel])
            self.assertEqual(len(findings), 1, findings)
            self.assertIn("plugin_redundant_prod", findings[0]["message"])

    def test_preexisting_vs_newly_redundant_at_change_base(self):
        rel = "src/foo/BUILD.bazel"
        preexisting_comp = (
            "[\n"
            "    rustc_library(\n"
            "        name = n,\n"
            "        testonly = True,\n"
            '        srcs = ["src/" + n + ".rs"],\n'
            '        edition = "2024",\n'
            '        lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
            "    )\n"
            "    for n in [\n"
            '        "comp_a",\n'
            '        "comp_b",\n'
            "    ]\n"
            "]\n"
        )
        base_content = (
            HEADER
            + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
            + "rustc_library(\n"
            + '    name = "already_redundant",\n'
            + "    testonly = True,\n"
            + '    srcs = ["src/a.rs"],\n'
            + '    edition = "2024",\n'
            + "    lint_config = '@//build/config/rust/lints:clippy_warn_default',\n"
            + ")\n\n"
            + "rustc_library(\n"
            + '    name = "becomes_redundant",\n'
            + '    srcs = ["src/b.rs"],\n'
            + '    edition = "2024",\n'
            + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
            + ")\n\n"
            + preexisting_comp
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {rel: base_content})
            full = os.path.join(tmpdir, rel)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "already_redundant",\n'
                    + "    testonly = True,\n"
                    + '    srcs = ["src/a.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + ")\n\n"
                    + "rustc_library(\n"
                    + '    name = "becomes_redundant",\n'
                    + "    testonly = True,\n"
                    + '    srcs = ["src/b.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + ")\n\n"
                    + preexisting_comp
                    + "\n[\n"
                    + "    rustc_library(\n"
                    + "        name = m,\n"
                    + "        testonly = True,\n"
                    + '        srcs = ["src/" + m + ".rs"],\n'
                    + '        edition = "2024",\n'
                    + '        lint_config = "//build/config/rust/lints:clippy_warn_default",\n'
                    + "    )\n"
                    + "    for m in [\n"
                    + '        "new_a",\n'
                    + '        "new_b",\n'
                    + "    ]\n"
                    + "]\n"
                )
            findings = _run_minimality(tmpdir, base, "src/foo", [rel])
            self.assertEqual(len(findings), 2, findings)
            self.assertIn("becomes_redundant", findings[0]["message"])
            self.assertIn("Target '<unnamed>'", findings[1]["message"])


if __name__ == "__main__":
    unittest.main()
