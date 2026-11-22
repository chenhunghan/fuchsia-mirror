#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for bazel_minimality.sh."""

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
    _git(tmpdir, "config", "user.email", "test")
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
            redundant = [
                f
                for f in findings
                if f["category"] == "redundant_default_attribute"
            ]
            names = [f["message"] for f in redundant]
            self.assertEqual(len(redundant), 3, redundant)
            self.assertTrue(any("prod_lib" in m for m in names))
            self.assertTrue(any("prod_idk_tool" in m for m in names))
            self.assertTrue(any("local_testonly" in m for m in names))
            config_forbidden = [
                f
                for f in findings
                if f["category"] == "build_config_modification_forbidden"
            ]
            self.assertEqual(len(config_forbidden), 1, findings)
            self.assertEqual(config_forbidden[0]["file"], lints_rel)

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


class BazelMinimalityPackageNameTest(unittest.TestCase):
    def test_redundant_package_name(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            rel = "src/foo/BUILD.bazel"
            full = os.path.join(tmpdir, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")\n'
                    + 'load("//build/bazel/rules/testing:fx_unittest_package.bzl", "fx_unittest_package")\n\n'
                    + "fx_package(\n"
                    + '    name = "foo",\n'
                    + '    package_name = "foo",\n'
                    + '    components = [":foo_component"],\n'
                    + ")\n\n"
                    + "fx_package(\n"
                    + '    name = "foo_renamed",\n'
                    + '    package_name = "foo-renamed",\n'
                    + '    components = [":foo_component"],\n'
                    + ")\n\n"
                    + "fx_unittest_package(\n"
                    + '    name = "foo_tests",\n'
                    + '    package_name = "foo_tests",\n'
                    + '    unit_tests = [":foo_test"],\n'
                    + ")\n"
                )
            findings = _run_minimality(tmpdir, base, "src/foo", [rel])
            self.assertEqual(
                [(f["category"], f["line"]) for f in findings],
                [
                    ("redundant_default_attribute", 10),
                    ("redundant_default_attribute", 22),
                ],
                findings,
            )
            self.assertIn("(fx_package)", findings[0]["message"])
            self.assertIn("(fx_unittest_package)", findings[1]["message"])

    def test_preexisting_redundant_package_name(self):
        rel = "src/foo/BUILD.bazel"
        content = (
            HEADER
            + 'load("//build/bazel/rules/packages:fx_package.bzl", "fx_package")\n\n'
            + "fx_package(\n"
            + '    name = "foo",\n'
            + '    package_name = "foo",\n'
            + '    components = [":foo_component"],\n'
            + ")\n"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {rel: content})
            with open(os.path.join(tmpdir, rel), "a", encoding="utf-8") as f:
                f.write(
                    "\nfx_package(\n"
                    + '    name = "bar",\n'
                    + '    components = [":bar_component"],\n'
                    + ")\n"
                )
            findings = _run_minimality(tmpdir, base, "src/foo", [rel])
            self.assertEqual(findings, [], findings)


class BazelMinimalitySquashedLessonsTest(unittest.TestCase):
    def test_unsynced_rust_test_crate_name_and_stale_comment(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/unsynced/BUILD.gn": HEADER
                    + "# Bazel-only host tool\n",
                    "src/synced/BUILD.gn": (
                        HEADER
                        + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    ),
                },
            )
            unsynced_rel = "src/unsynced/BUILD.bazel"
            os.makedirs(os.path.join(tmpdir, "src/unsynced"), exist_ok=True)
            with open(
                os.path.join(tmpdir, unsynced_rel), "w", encoding="utf-8"
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n\n'
                    + "# Keep crate_name so GN output_name stays foo_lib_test.\n"
                    + "host_rustc_test(\n"
                    + '    name = "lib_test",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    crate_name = "foo_lib_test",\n'
                    + '    edition = "2024",\n'
                    + ")\n"
                )

            synced_rel = "src/synced/BUILD.bazel"
            os.makedirs(os.path.join(tmpdir, "src/synced"), exist_ok=True)
            with open(
                os.path.join(tmpdir, synced_rel), "w", encoding="utf-8"
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n\n'
                    + "host_rustc_test(\n"
                    + '    name = "lib_test",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    crate_name = "foo_lib_test",\n'
                    + '    edition = "2024",\n'
                    + ")\n"
                )

            findings = _run_minimality(
                tmpdir,
                base,
                "src/unsynced,src/synced",
                [unsynced_rel, synced_rel],
            )
            cats = [(f["file"], f["category"]) for f in findings]
            self.assertEqual(
                cats,
                [
                    (unsynced_rel, "redundant_default_attribute"),
                    (unsynced_rel, "stale_test_crate_name_comment"),
                ],
                findings,
            )

    def test_collateral_upstream_visibility_and_host_os_constraints_allowed(
        self,
    ):
        dep_rel = "src/dep/BUILD.bazel"
        foo_rel = "src/foo/BUILD.bazel"
        base_dep = (
            HEADER
            + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
            + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n\n'
            + "rustc_library(\n"
            + '    name = "dep",\n'
            + '    srcs = ["src/lib.rs"],\n'
            + '    edition = "2024",\n'
            + "    target_compatible_with = HOST_CONSTRAINTS,\n"
            + '    visibility = ["//src/other:__pkg__"],\n'
            + ")\n"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    dep_rel: base_dep,
                    "src/dep/src/lib.rs": "",
                    "src/foo/src/lib.rs": "",
                },
            )
            with open(
                os.path.join(tmpdir, dep_rel), "w", encoding="utf-8"
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                    + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "dep",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + "    visibility = [\n"
                    + '        "//src/foo:__pkg__",\n'
                    + '        "//src/other:__pkg__",\n'
                    + "    ],\n"
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, foo_rel), "w", encoding="utf-8"
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                    + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "foo",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + '    deps = ["//src/dep"],\n'
                    + ")\n"
                )
            findings = _run_minimality(
                tmpdir, base, "src/foo", [dep_rel, foo_rel]
            )
            self.assertEqual(findings, [], findings)

    def test_host_rustc_test_redundant_edition_next_to_crate(self):
        rel = "src/bar/BUILD.bazel"
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/bar/src/lib.rs": "",
                    "src/bar/BUILD.gn": 'rustc_library("bar") {}\n',
                },
            )
            full = os.path.join(tmpdir, rel)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n'
                    + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "bar",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n\n"
                    + "# @bazel2gn:skip\n"
                    + "host_rustc_test(\n"
                    + '    name = "bar_test",\n'
                    + '    crate = ":bar",\n'
                    + '    edition = "2024",\n'
                    + ")\n"
                )
            findings = _run_minimality(tmpdir, base, "src/bar", [rel])
            cats = [(f["file"], f["category"]) for f in findings]
            self.assertEqual(
                cats, [(rel, "redundant_default_attribute")], findings
            )
            self.assertIn("(host_rustc_test)", findings[0]["message"])

    def test_unified_transitive_legacy_host_constraints_skips_proc_macro(self):
        tool_bz = "src/developer/ffx/tools/auth/BUILD.bazel"
        tool_gn = "src/developer/ffx/tools/auth/BUILD.gn"
        pm_bz = "src/lib/pm_dep/BUILD.bazel"
        reg_bz = "src/lib/reg_dep/BUILD.bazel"
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "bundles/infra/test/BUILD.gn": (
                        '_all_host_tools = [ "//src/developer/ffx/tools/auth:ffx_auth" ]\n'
                    ),
                    pm_bz: (
                        HEADER
                        + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
                        + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_proc_macro")\n\n'
                        + "rustc_proc_macro(\n"
                        + '    name = "pm_dep",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + '    edition = "2024",\n'
                        + "    target_compatible_with = HOST_CONSTRAINTS,\n"
                        + ")\n"
                    ),
                    reg_bz: (
                        HEADER
                        + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
                        + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n\n'
                        + "rustc_library(\n"
                        + '    name = "reg_dep",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + '    edition = "2024",\n'
                        + "    target_compatible_with = HOST_CONSTRAINTS,\n"
                        + ")\n"
                    ),
                    tool_gn: 'ffx_tool("ffx_auth") {}\n',
                },
            )
            os.makedirs(
                os.path.dirname(os.path.join(tmpdir, tool_bz)), exist_ok=True
            )
            with open(
                os.path.join(tmpdir, tool_bz), "w", encoding="utf-8"
            ) as f:
                f.write(
                    HEADER
                    + 'load("//src/developer/ffx/build:ffx_tool.bzl", "idk_ffx_tool")\n\n'
                    + "idk_ffx_tool(\n"
                    + '    name = "ffx_auth",\n'
                    + '    srcs = ["src/main.rs"],\n'
                    + '    edition = "2024",\n'
                    + "    deps = [\n"
                    + '        "//src/lib/pm_dep",\n'
                    + '        "//src/lib/reg_dep",\n'
                    + "    ],\n"
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, tool_gn), "w", encoding="utf-8"
            ) as f:
                f.write(
                    'bazel_ffx_tool("ffx_auth") {\n'
                    '  bazel_target = ":ffx_auth"\n'
                    "}\n"
                )
            findings = _run_minimality(
                tmpdir,
                base,
                "src/developer/ffx/tools/auth",
                [tool_bz, tool_gn],
            )
            cats = [f["category"] for f in findings]
            self.assertEqual(
                cats, ["transitive_legacy_host_constraints"], findings
            )
            self.assertIn("//src/lib/reg_dep:reg_dep", findings[0]["message"])
            self.assertNotIn("pm_dep", findings[0]["message"])


class MachineryRound1FixesTest(unittest.TestCase):
    def test_inspect_gn_deps_var_table_package_group_foreach_and_fidl(self):
        mach_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        inspect_sh = os.path.join(mach_dir, "tools", "inspect_gn_deps.sh")
        with tempfile.TemporaryDirectory() as tmpdir:
            files = {
                "build/rust/fidl_rust_next.bzl": (
                    HEADER
                    + 'fidl_rust_next_allowlist = ["//src/allowed:__subpackages__"]\n'
                ),
                "src/dep_var/BUILD.bazel": (
                    HEADER
                    + '_VIS = ["//visibility:public"]\n'
                    + 'rustc_library(name = "foo", visibility = _VIS)\n'
                ),
                "src/dep_pg/BUILD.bazel": (
                    HEADER
                    + "package_group(\n"
                    + '    name = "grp",\n'
                    + '    packages = ["-//src/caller/secret/...", "//src/caller/..."],\n'
                    + ")\n"
                    + 'rustc_library(name = "bar", visibility = [":grp"])\n'
                ),
                "src/dep_b/BUILD.bazel": (
                    HEADER
                    + 'rustc_library(name = "dep_b", visibility = ["//visibility:public"])\n'
                ),
                "src/dep_c/BUILD.bazel": (
                    HEADER
                    + 'rustc_library(name = "dep_c", visibility = ["//visibility:public"])\n'
                ),
                "sdk/fidl/fuchsia.sample/BUILD.bazel": (
                    HEADER
                    + "fidl_library(\n"
                    + '    name = "fuchsia.sample",\n'
                    + '    visibility = ["//visibility:public"],\n'
                    + ")\n"
                ),
                "src/caller/secret/BUILD.gn": (
                    'root_prefix = "//src"\n'
                    'extra_deps = [ "//src/dep_var:foo" ]\n'
                    'extra_deps += [ "$root_prefix/dep_b" ]\n'
                    'foreach (sub, [ "dep_c" ]) {\n'
                    '  rustc_library("lib_${sub}") {\n'
                    "    deps = extra_deps + [\n"
                    '      "//src/$sub",\n'
                    '      "//src/dep_pg:bar",\n'
                    '      "//sdk/fidl/fuchsia.sample:fuchsia.sample_cpp_wire",\n'
                    '      "//sdk/fidl/fuchsia.sample:fuchsia.sample_hlcpp",\n'
                    '      "//sdk/fidl/fuchsia.sample:fuchsia.sample_banjo_cpp",\n'
                    '      "//sdk/fidl/fuchsia.sample:fuchsia.sample_rust_next",\n'
                    "    ]\n"
                    "  }\n"
                    "}\n"
                ),
            }
            _init_repo(tmpdir, files)
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env["PLANTER_WORKDIR"] = tmpdir
            out = subprocess.run(
                ["bash", inspect_sh, "src/caller/secret"],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            rep = json.loads(out.stdout)
            ready = rep["ready_deps"]
            self.assertIn(
                "//src/dep_var:foo -> //src/dep_var:foo (rustc_library)", ready
            )
            self.assertIn(
                "//src/dep_b:dep_b -> //src/dep_b (rustc_library)", ready
            )
            self.assertIn(
                "//src/dep_c:dep_c -> //src/dep_c (rustc_library)", ready
            )
            self.assertIn(
                "//sdk/fidl/fuchsia.sample:fuchsia.sample_cpp_wire -> "
                "//sdk/fidl/fuchsia.sample:fuchsia.sample_cpp (fidl_library)",
                ready,
            )
            self.assertFalse(any("dep_pg:bar" in r for r in ready), ready)
            self.assertFalse(any("_rust_next" in r for r in ready), ready)

            vis_targets = {
                (e["gn_label"], e["bazel_build"], e["target"])
                for e in rep["visibility_edits_needed"]
            }
            self.assertIn(
                ("//src/dep_pg:bar", "src/dep_pg/BUILD.bazel", "grp"),
                vis_targets,
            )
            self.assertIn(
                (
                    "//sdk/fidl/fuchsia.sample:fuchsia.sample_rust_next",
                    "build/rust/fidl_rust_next.bzl",
                    "fidl_rust_next_allowlist",
                ),
                vis_targets,
            )

            mismatches = {m["gn_label"] for m in rep["target_name_mismatches"]}
            self.assertIn(
                "//sdk/fidl/fuchsia.sample:fuchsia.sample_hlcpp", mismatches
            )
            self.assertIn(
                "//sdk/fidl/fuchsia.sample:fuchsia.sample_banjo_cpp", mismatches
            )

    def test_machinery_script_lint_workdir_exports_dedup_and_parent_layout(
        self,
    ):
        mach_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        lint_check = os.path.join(
            mach_dir, "checks", "machinery_script_lint.sh"
        )
        lint_tool = os.path.join(mach_dir, "tools", "lint_machinery_scripts.sh")
        with tempfile.TemporaryDirectory() as tmpdir:
            ws_dir = os.path.join(tmpdir, "ws")
            os.makedirs(ws_dir, exist_ok=True)
            parent_proj = os.path.join(tmpdir, "parent_proj")
            os.makedirs(
                os.path.join(parent_proj, "machinery", "checks"), exist_ok=True
            )
            os.makedirs(
                os.path.join(parent_proj, "machinery", "tools"), exist_ok=True
            )
            os.makedirs(
                os.path.join(parent_proj, "machinery", "prompts"), exist_ok=True
            )
            with open(
                os.path.join(parent_proj, "machinery", "prompts", "coder.md"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write("# coder\n")
            with open(lint_check, encoding="utf-8") as src_f, open(
                os.path.join(
                    parent_proj,
                    "machinery",
                    "checks",
                    "machinery_script_lint.sh",
                ),
                "w",
                encoding="utf-8",
            ) as dst_f:
                dst_f.write(src_f.read())
            with open(lint_tool, encoding="utf-8") as src_f, open(
                os.path.join(
                    parent_proj,
                    "machinery",
                    "tools",
                    "lint_machinery_scripts.sh",
                ),
                "w",
                encoding="utf-8",
            ) as dst_f:
                dst_f.write(src_f.read())
            with open(
                os.path.join(
                    parent_proj, "machinery", "checks", "planter_check_lib.py"
                ),
                "w",
                encoding="utf-8",
            ) as f:
                f.write("def old_helper():\n    return 1\n")

            base = _init_repo(
                ws_dir,
                {
                    "build/beads/planter/machinery/checks/planter_check_lib.py": (
                        "def old_helper():\n    return 1\n"
                    )
                },
            )
            pcl_ws = os.path.join(
                ws_dir,
                "build/beads/planter/machinery/checks/planter_check_lib.py",
            )
            sample_sh = os.path.join(
                ws_dir, "build/beads/planter/machinery/checks/sample_check.sh"
            )
            with open(pcl_ws, "w", encoding="utf-8") as f:
                f.write(
                    "def old_helper():\n    return 1\n\n"
                    "def newly_added_helper():\n    return 2\n"
                )
            with open(sample_sh, "w", encoding="utf-8") as f:
                f.write(
                    "#!/usr/bin/env bash\n"
                    "python3 - <<'PYEOF'\n"
                    "import planter_check_lib as pcl\n"
                    "print(pcl.newly_added_helper())\n"
                    "PYEOF\n"
                )

            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": ws_dir,
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_PROJECT_DIR": parent_proj,
                    "PLANTER_LINT_PROJECT_DIR": "0",
                    "PLANTER_TARGET_DIRS": "//src/a, //src/b",
                    "PLANTER_TARGET_DIR": "src/a",
                }
            )
            res_first = subprocess.run(
                [
                    "bash",
                    os.path.join(
                        parent_proj,
                        "machinery",
                        "checks",
                        "machinery_script_lint.sh",
                    ),
                ],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(json.loads(res_first.stdout), [])

            # Also verify parent PLANTER_PROJECT_DIR invocation via lint_machinery_scripts.sh.
            env_tool = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env_tool["PLANTER_WORKDIR"] = ws_dir
            env_tool["PLANTER_CHANGE_BASE"] = base
            env_tool["PLANTER_PROJECT_DIR"] = parent_proj
            res_tool = subprocess.run(
                [
                    "bash",
                    os.path.join(
                        parent_proj,
                        "machinery",
                        "tools",
                        "lint_machinery_scripts.sh",
                    ),
                ],
                env=env_tool,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(json.loads(res_tool.stdout), [])

            # Verify comma-delimited dedup guard still runs on first dir when broken.
            with open(sample_sh, "w", encoding="utf-8") as f:
                f.write(
                    "#!/usr/bin/env bash\n"
                    "python3 - <<'PYEOF'\n"
                    "import planter_check_lib as pcl\n"
                    "print(pcl.nonexistent_helper())\n"
                    "PYEOF\n"
                )
            res_bad = subprocess.run(
                [
                    "bash",
                    os.path.join(
                        parent_proj,
                        "machinery",
                        "checks",
                        "machinery_script_lint.sh",
                    ),
                ],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            bad_list = json.loads(res_bad.stdout)
            self.assertEqual(len(bad_list), 1, bad_list)
            self.assertEqual(
                bad_list[0]["category"], "machinery_undefined_python_name"
            )

            # And skips cleanly on the second dir.
            env["PLANTER_TARGET_DIR"] = "src/b"
            res_second = subprocess.run(
                [
                    "bash",
                    os.path.join(
                        parent_proj,
                        "machinery",
                        "checks",
                        "machinery_script_lint.sh",
                    ),
                ],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(json.loads(res_second.stdout), [])

    def test_machinery_script_lint_class_super_implicit_cell(self):
        mach_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        lint_check = os.path.join(
            mach_dir, "checks", "machinery_script_lint.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "build/beads/planter/machinery/checks/sample_test.py": (
                        "x = 1\n"
                    ),
                },
            )
            sample_py = os.path.join(
                tmpdir,
                "build",
                "beads",
                "planter",
                "machinery",
                "checks",
                "sample_test.py",
            )
            with open(sample_py, "w", encoding="utf-8") as f:
                f.write(
                    "import unittest\n\n"
                    "if True:\n"
                    "    COND_VAL: int = 1\n\n\n"
                    "class SampleTest(unittest.TestCase):\n"
                    "    LocalType = int\n"
                    "    count: LocalType = 0\n"
                    "    qual = __qualname__\n\n"
                    "    \x40classmethod\n"
                    "    def setUpClass(cls) -> None:\n"
                    "        super().setUpClass()\n"
                    "        cls.count = COND_VAL\n\n"
                    "    def helper(self, val: LocalType) -> LocalType:\n"
                    "        _ = __class__\n"
                    "        return val\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_PROJECT_DIR": mach_dir,
                    "PLANTER_LINT_PROJECT_DIR": "0",
                }
            )
            res_ok = subprocess.run(
                ["bash", lint_check],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(json.loads(res_ok.stdout), [])

            with open(sample_py, "a", encoding="utf-8") as f:
                f.write("\n\ndef outside_class():\n    return __class__\n")
            res_err = subprocess.run(
                ["bash", lint_check],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            err_list = json.loads(res_err.stdout)
            self.assertEqual(len(err_list), 1, err_list)
            self.assertEqual(
                err_list[0]["category"], "machinery_undefined_python_name"
            )
            self.assertIn("__class__", err_list[0]["message"])

    def test_sentinel_restore_ref_uses_base_revision(self):
        ms_sh = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "migration_sanity.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            anc_gn = "src/anc/BUILD.gn"
            anc_bz = "src/anc/BUILD.bazel"
            main_gn = "src/main_pkg/BUILD.gn"
            main_bz = "src/main_pkg/BUILD.bazel"
            base_anc_gn = (
                'host_test("anc_host_test") {\n'
                '  binary_path = "$root_out_dir/anc"\n'
                "}\n\n"
                "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
            )
            base_anc_bz = (
                HEADER
                + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n\n'
                + "# @bazel2gn:skip\n"
                + "host_rustc_test(\n"
                + '    name = "anc_host_test",\n'
                + '    srcs = ["src/lib.rs"],\n'
                + '    edition = "2024",\n'
                + '    test_data = [":fixture"],\n'
                + ")\n"
            )
            _init_repo(
                tmpdir,
                {
                    anc_gn: base_anc_gn,
                    anc_bz: base_anc_bz,
                    "src/anc/src/lib.rs": "",
                    main_gn: 'rustc_library("main_pkg") {\n  sources = ["src/lib.rs"]\n}\n',
                    "src/main_pkg/src/lib.rs": "",
                },
            )
            # Commit deletion at HEAD so PLANTER_CHANGE_BASE=HEAD~1 exercises base_sentinel_rev.
            with open(os.path.join(tmpdir, anc_gn), "w", encoding="utf-8") as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                )
            with open(
                os.path.join(tmpdir, main_bz), "w", encoding="utf-8"
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "main_pkg",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, main_gn), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("main_pkg") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    '  edition = "2024"\n'
                    "}\n"
                )
            _git(tmpdir, "add", ".")
            _git(tmpdir, "commit", "-q", "-m", "migrate")

            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/main_pkg",
                    "PLANTER_TARGET_DIRS": "src/main_pkg",
                    "PLANTER_CHANGE_BASE": "HEAD~1",
                    "PLANTER_RESOLVED_CHANGE_BASE": "HEAD~1",
                }
            )
            out = subprocess.run(
                ["bash", ms_sh],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            findings = json.loads(out.stdout)
            drop_findings = [
                f
                for f in findings
                if f["category"] == "dropped_dual_build_gn_test_above_sentinel"
            ]
            self.assertEqual(len(drop_findings), 1, findings)
            self.assertIn(
                "git checkout HEAD~1 -- src/anc/BUILD.gn",
                drop_findings[0]["remediation"],
            )


class MachineryRound2FixesTest(unittest.TestCase):
    """Regression tests for Round 2 machinery review fixes."""

    def test_host_defs_bzl_validation_allowed_while_legacy_macro_fires(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            host_defs = (
                HEADER
                + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
                + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_binary")\n\n'
                + "def _impl(name, target_compatible_with, **kwargs):\n"
                + "    if target_compatible_with != HOST_CONSTRAINTS and target_compatible_with != HOST_OS_CONSTRAINTS:\n"
                + '        fail("bad")\n'
                + "    rustc_binary(name = name, target_compatible_with = target_compatible_with, **kwargs)\n\n"
                + "rustc_binary_host_tool = macro(\n"
                + "    implementation = _impl,\n"
                + "    attrs = {\n"
                + '        "target_compatible_with": attr.string_list(default = HOST_OS_CONSTRAINTS),\n'
                + "    },\n"
                + ")\n"
            )
            legacy_defs = (
                HEADER
                + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
                + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                + "def legacy_host_lib(name, **kwargs):\n"
                + "    rustc_library(name = name, target_compatible_with = HOST_CONSTRAINTS, **kwargs)\n"
            )
            base = _init_repo(
                tmpdir,
                {
                    "bundles/infra/test/BUILD.gn": (
                        'deps = [ "//tools/my_tool:my_tool_host($host_arm64_toolchain)" ]\n'
                    ),
                    "build/bazel/rules/host/defs.bzl": host_defs,
                    "src/legacy/defs.bzl": legacy_defs,
                    "src/legacy/BUILD.bazel": (
                        HEADER
                        + 'load("//src/legacy:defs.bzl", "legacy_host_lib")\n\n'
                        + 'legacy_host_lib(name = "legacy_lib", srcs = ["lib.rs"])\n'
                    ),
                    "src/legacy/lib.rs": "",
                    "tools/my_tool/BUILD.gn": (
                        'rustc_binary("my_tool") {\n  sources = ["src/main.rs"]\n}\n'
                    ),
                    "tools/my_tool/src/main.rs": "fn main() {}\n",
                },
            )
            tool_bz = os.path.join(tmpdir, "tools/my_tool/BUILD.bazel")
            tool_gn = os.path.join(tmpdir, "tools/my_tool/BUILD.gn")
            with open(tool_gn, "w", encoding="utf-8") as f:
                f.write(
                    'if (is_host) {\n  bazel_host_tool("my_tool_host") {\n'
                    '    bazel_target = "//tools/my_tool:my_tool"\n  }\n}\n'
                )
            with open(tool_bz, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host:defs.bzl", "rustc_binary_host_tool")\n\n'
                    + 'package(default_visibility = ["//visibility:public"])\n\n'
                    + "rustc_binary_host_tool(\n"
                    + '    name = "my_tool",\n'
                    + '    edition = "2024",\n'
                    + '    srcs = ["src/main.rs"],\n'
                    + ")\n"
                )
            ok_findings = _run_minimality(
                tmpdir,
                base,
                "tools/my_tool",
                [
                    "tools/my_tool/BUILD.bazel",
                    "tools/my_tool/BUILD.gn",
                ],
            )
            self.assertEqual(
                [
                    f
                    for f in ok_findings
                    if f["category"] == "transitive_legacy_host_constraints"
                ],
                [],
                ok_findings,
            )

            # Now depend on //src/legacy:legacy_lib whose macro sets HOST_CONSTRAINTS.
            with open(tool_bz, "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host:defs.bzl", "rustc_binary_host_tool")\n\n'
                    + 'package(default_visibility = ["//visibility:public"])\n\n'
                    + "rustc_binary_host_tool(\n"
                    + '    name = "my_tool",\n'
                    + '    edition = "2024",\n'
                    + '    srcs = ["src/main.rs"],\n'
                    + '    deps = ["//src/legacy:legacy_lib"],\n'
                    + ")\n"
                )
            bad_findings = _run_minimality(
                tmpdir,
                base,
                "tools/my_tool",
                [
                    "tools/my_tool/BUILD.bazel",
                    "tools/my_tool/BUILD.gn",
                ],
            )
            hc_findings = [
                f
                for f in bad_findings
                if f["category"] == "transitive_legacy_host_constraints"
            ]
            self.assertEqual(len(hc_findings), 1, bad_findings)
            self.assertIn("src/legacy/defs.bzl", hc_findings[0]["message"])
            self.assertNotIn(
                "build/bazel/rules/host/defs.bzl", hc_findings[0]["message"]
            )

    def test_unsynced_rust_test_crate_name_remediation_no_double_suffix(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            pkg = "src/developer/ffx/plugins/foo"
            base = _init_repo(
                tmpdir,
                {
                    f"{pkg}/BUILD.gn": 'ffx_plugin("ffx_foo") {\n  with_unit_tests = true\n}\n',
                    f"{pkg}/src/lib.rs": "",
                    f"{pkg}/src/args.rs": "",
                },
            )
            with open(
                os.path.join(tmpdir, f"{pkg}/BUILD.gn"), "w", encoding="utf-8"
            ) as f:
                f.write('group("tests") {\n  testonly = true\n}\n')
            with open(
                os.path.join(tmpdir, f"{pkg}/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n'
                    + 'load("//src/developer/ffx/build:ffx_plugin.bzl", "ffx_plugin")\n\n'
                    + 'package(default_visibility = ["//visibility:public"])\n\n'
                    + "ffx_plugin(\n"
                    + '    name = "ffx_foo",\n'
                    + '    args_srcs = ["src/args.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + ")\n\n"
                    + "host_rustc_test(\n"
                    + '    name = "ffx_foo_test",\n'
                    + '    crate = ":ffx_foo",\n'
                    + '    crate_name = "ffx_foo_test",\n'
                    + ")\n"
                )
            findings = _run_minimality(
                tmpdir,
                base,
                pkg,
                [f"{pkg}/BUILD.bazel", f"{pkg}/BUILD.gn"],
            )
            rda = [
                f
                for f in findings
                if f["category"] == "redundant_default_attribute"
            ]
            self.assertEqual(len(rda), 1, findings)
            self.assertNotIn("ffx_foo_test_test", rda[0]["remediation"])
            self.assertNotIn("rename the target", rda[0]["remediation"])

    def test_gn_attr_parity_test_args_sentinel_free_bridge_vs_sentinel_sync(
        self,
    ):
        attr_sh = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "gn_attr_parity.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            pkg = "src/developer/ffx/plugins/foo"
            base = _init_repo(
                tmpdir,
                {
                    f"{pkg}/BUILD.gn": (
                        'ffx_plugin("ffx_foo") {\n'
                        "  with_unit_tests = true\n"
                        '  args = [ "--nocapture" ]\n'
                        "}\n"
                    ),
                    f"{pkg}/src/lib.rs": "",
                },
            )
            with open(
                os.path.join(tmpdir, f"{pkg}/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n'
                    + 'load("//src/developer/ffx/build:ffx_plugin.bzl", "ffx_plugin")\n\n'
                    + 'ffx_plugin(name = "ffx_foo", srcs = ["src/lib.rs"], edition = "2024")\n'
                    + 'host_rustc_test(name = "ffx_foo_test", crate = ":ffx_foo", test_args = ["--nocapture"])\n'
                )
            with open(
                os.path.join(tmpdir, f"{pkg}/BUILD.gn"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "if (is_host) {\n"
                    '  bazel_ffx_tool("bin") {\n'
                    '    bazel_target = "//src/developer/ffx/plugins/foo:bin"\n'
                    "  }\n"
                    '  bazel_test_suite("host_tests") {\n'
                    '    bazel_test_suite_target = "//src/developer/ffx/plugins/foo:host_tests"\n'
                    "  }\n"
                    "}\n"
                    'group("tests") {\n  testonly = true\n  deps = [ ":host_tests($host_toolchain)" ]\n}\n'
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": pkg,
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_RESOLVED_CHANGE_BASE": base,
                    "PLANTER_SKIP_DIFF_COVERAGE": "1",
                }
            )
            res_bridge = subprocess.run(
                ["bash", attr_sh],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            bridge_findings = json.loads(res_bridge.stdout)
            self.assertEqual(
                [
                    f
                    for f in bridge_findings
                    if f["category"] == "test_args_dropped_or_modified"
                ],
                [],
                bridge_findings,
            )

            # With BAZEL2GN SENTINEL and unskipped test, missing GN args must fire.
            with open(
                os.path.join(tmpdir, f"{pkg}/BUILD.gn"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_test("ffx_foo_test") {\n  sources = [ "src/lib.rs" ]\n}\n'
                )
            res_sent = subprocess.run(
                ["bash", attr_sh],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            sent_findings = json.loads(res_sent.stdout)
            self.assertEqual(
                len(
                    [
                        f
                        for f in sent_findings
                        if f["category"] == "test_args_dropped_or_modified"
                    ]
                ),
                1,
                sent_findings,
            )

    def test_gn_dep_parity_outer_foreach_plus_equals_and_var_interp(self):
        dep_sh = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "gn_dep_parity.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/sub/a/BUILD.gn": (
                        'config("cfg") { ldflags = [ "-lm" ] }\n'
                        'rustc_library("lib") {\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        '  public_configs = [ ":cfg" ]\n'
                        "}\n"
                    ),
                    "src/sub/b/BUILD.gn": (
                        'rustc_library("lib") { sources = [ "src/lib.rs" ] }\n'
                    ),
                    "src/pkg/BUILD.gn": (
                        'prefix = "//src/sub"\n'
                        'subs = [ "a" ]\n'
                        'subs += [ "b" ]\n'
                        "if (is_host) {\n"
                        "  foreach(s, subs) {\n"
                        '    rustc_library("foo_${s}") {\n'
                        '      sources = [ "src/lib.rs" ]\n'
                        '      deps = [ "${prefix}/${s}:lib" ]\n'
                        "    }\n"
                        "  }\n"
                        "}\n"
                    ),
                    "src/pkg/src/lib.rs": "",
                },
            )
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.gn"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("foo_a") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    '  deps = [ "//src/sub/a:lib" ]\n'
                    "}\n"
                    'rustc_library("foo_b") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    '  deps = [ "//src/sub/b:lib" ]\n'
                    "}\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/pkg",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_RESOLVED_CHANGE_BASE": base,
                }
            )
            res_ok = subprocess.run(
                ["bash", dep_sh],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(json.loads(res_ok.stdout), [])

            # Dropping //src/sub/a:lib (which carries public_configs) from foo_a must fire.
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.gn"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("foo_a") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    "  deps = []\n"
                    "}\n"
                    'rustc_library("foo_b") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    '  deps = [ "//src/sub/b:lib" ]\n'
                    "}\n"
                )
            res_drop = subprocess.run(
                ["bash", dep_sh],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            drop_findings = json.loads(res_drop.stdout)
            self.assertEqual(
                [f["category"] for f in drop_findings],
                ["dep_link_settings_dropped"],
                drop_findings,
            )


class MachineryRound3FixesTest(unittest.TestCase):
    def test_machinery_script_lint_guarded_imports_and_none_fallback(self):
        mach_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        lint_check = os.path.join(
            mach_dir, "checks", "machinery_script_lint.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            bad_py = os.path.join(tmpdir, "guarded_bad.py")
            with open(bad_py, "w", encoding="utf-8") as f:
                f.write(
                    "try:\n"
                    "    import planter_check_lib as pcl\n"
                    "except ImportError:\n"
                    "    pcl = None\n\n"
                    "def run():\n"
                    "    if pcl is not None:\n"
                    "        return pcl.nonexistent_helper()\n"
                    "    return None\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env["PLANTER_WORKDIR"] = tmpdir
            env["PLANTER_PROJECT_DIR"] = mach_dir
            res_bad = subprocess.run(
                ["bash", lint_check, "--no-project-dir", bad_py],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            bad_findings = json.loads(res_bad.stdout)
            self.assertEqual(
                [f["category"] for f in bad_findings],
                ["machinery_undefined_python_name"],
                bad_findings,
            )

            good_py = os.path.join(tmpdir, "guarded_good.py")
            with open(good_py, "w", encoding="utf-8") as f:
                f.write(
                    "try:\n"
                    "    import planter_check_lib as pcl\n"
                    "except ImportError:\n"
                    "    pcl = None\n\n"
                    "def run(txt):\n"
                    "    return pcl.blank_comments(txt) if pcl else txt\n"
                )
            res_good = subprocess.run(
                ["bash", lint_check, "--no-project-dir", good_py],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            self.assertEqual(json.loads(res_good.stdout), [])

    def test_gn_dep_parity_target_scope_isolation_and_fidl_flex_crate(self):
        dep_sh = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "gn_dep_parity.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/a/BUILD.gn": (
                        'config("cfg_a") { ldflags = [ "-la" ] }\n'
                        'rustc_library("a") {\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        '  public_configs = [ ":cfg_a" ]\n'
                        "}\n"
                    ),
                    "src/b/BUILD.gn": (
                        'rustc_library("b") { sources = [ "src/lib.rs" ] }\n'
                    ),
                    "sdk/fidl/fuchsia.net/BUILD.gn": 'fidl("fuchsia.net") {}\n',
                    "sdk/fidl/fuchsia.net/BUILD.bazel": (
                        HEADER
                        + 'load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")\n'
                        + 'fidl_library(name = "fuchsia.net", srcs = ["net.fidl"])\n'
                    ),
                    "src/pkg/BUILD.gn": (
                        'test_types = [ "short", "long" ]\n'
                        "foreach(test_type, test_types) {\n"
                        '  fuchsia_unittest_component("comp_${test_type}") {\n'
                        '    test_type = "system"\n'
                        '    deps = [ "//src/a:a" ]\n'
                        "  }\n"
                        "}\n"
                        'rustc_library("first") {\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        '  local_deps = [ "//src/a:a" ]\n'
                        "  deps = local_deps\n"
                        "}\n"
                        'rustc_library("second") {\n'
                        '  sources = [ "src/other.rs" ]\n'
                        '  local_deps = [ "//src/b:b" ]\n'
                        "  deps = local_deps\n"
                        "}\n"
                    ),
                    "src/pkg/src/lib.rs": "use flex_fuchsia_net as _;\n",
                    "src/pkg/src/other.rs": "",
                },
            )
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n'
                    + "rustc_library(\n"
                    + '    name = "first",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    deps = ["//src/a:a", "//sdk/fidl/fuchsia.net:fuchsia.net_rust_flex"],\n'
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.gn"), "w", encoding="utf-8"
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'fuchsia_unittest_component("comp_short") {\n'
                    '  test_type = "system"\n'
                    "  deps = []\n"
                    "}\n"
                    'fuchsia_unittest_component("comp_long") {\n'
                    '  test_type = "system"\n'
                    '  deps = [ "//src/a:a" ]\n'
                    "}\n"
                    'rustc_library("first") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    "  deps = []\n"
                    "}\n"
                    'rustc_library("second") {\n'
                    '  sources = [ "src/other.rs" ]\n'
                    '  deps = [ "//src/b:b" ]\n'
                    "}\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/pkg",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_RESOLVED_CHANGE_BASE": base,
                }
            )
            res = subprocess.run(
                ["bash", dep_sh],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            findings = json.loads(res.stdout)
            cats = [(f["category"], f["message"]) for f in findings]
            self.assertFalse(
                any(c == "unexpected_rust_dep_added" for c, _ in cats), cats
            )
            dropped_msgs = [
                m for c, m in cats if c == "dep_link_settings_dropped"
            ]
            self.assertTrue(
                any("comp_short" in m for m in dropped_msgs), dropped_msgs
            )
            self.assertTrue(
                any('"first"' in m for m in dropped_msgs), dropped_msgs
            )

    def test_inspect_gn_deps_chained_vars_nested_foreach_and_no_target_clobber(
        self,
    ):
        import ast
        import re
        import sys

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.join(os.path.dirname(checks_dir), "tools")
        if checks_dir not in sys.path:
            sys.path.insert(0, checks_dir)
        import planter_check_lib as pcl

        with open(
            os.path.join(tools_dir, "inspect_gn_deps.sh"), encoding="utf-8"
        ) as sf:
            ig_txt = sf.read()
        m = re.search(
            r"python3[^\n]*<<'PYEOF'\n(.*?)\nPYEOF", ig_txt, flags=re.S
        )
        assert m is not None
        tree = ast.parse(m.group(1), filename="inspect_gn_deps.sh")
        wanted = {
            "DEP_LIST_ATTRS",
            "expand_gn_foreach_and_vars",
            "extract_gn_deps_and_inputs",
        }
        selected = [
            n
            for n in tree.body
            if (isinstance(n, ast.FunctionDef) and n.name in wanted)
            or (
                isinstance(n, ast.Assign)
                and any(
                    isinstance(t, ast.Name) and t.id in wanted
                    for t in n.targets
                )
            )
        ]
        ns = {"os": os, "re": re, "pcl": pcl}
        exec(
            compile(
                ast.Module(body=selected, type_ignores=[]),
                "inspect_gn_deps.sh",
                "exec",
            ),
            ns,
        )
        fn = ns["extract_gn_deps_and_inputs"]
        gn_sample = (
            'a = "//sdk/fidl"\n'
            'b = "$a/fuchsia.net"\n'
            'rustc_binary("o") {\n'
            '  name = "unrelated"\n'
            '  local_deps = [ "$b:fuchsia.net_rust" ]\n'
            "  deps = local_deps\n"
            "}\n"
            'rustc_binary("p") {\n'
            '  local_deps = [ "//src/other:lib" ]\n'
            "  deps = local_deps\n"
            "}\n"
            'foreach(name, ["foo", "bar"]) {\n'
            '  sub = "//src/${name}"\n'
            '  foreach(mode, ["sync"]) {\n'
            '    rustc_test("t_${name}_${mode}") {\n'
            '      deps = [ "${sub}:${mode}" ]\n'
            "    }\n"
            "  }\n"
            "}\n"
        )
        deps, _ = fn(gn_sample, "src/caller")
        self.assertEqual(
            deps,
            [
                "//sdk/fidl/fuchsia.net:fuchsia.net_rust",
                "//src/other:lib",
                "//src/foo:sync",
                "//src/bar:sync",
            ],
        )

    def test_host_os_constraints_upgrade_rejects_unbacked_or_piggybacked_edits(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "tools/my_tool/BUILD.gn": 'go_binary("my_tool") {}\n',
                    "src/unbacked/BUILD.bazel": (
                        HEADER
                        + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                        + 'rustc_library(name = "u", srcs = ["lib.rs"], edition = "2024")\n'
                    ),
                    "src/piggyback/BUILD.bazel": (
                        HEADER
                        + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
                        + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                        + "rustc_library(\n"
                        + '    name = "p",\n'
                        + '    srcs = ["lib.rs"],\n'
                        + '    crate_features = ["base_feat"],\n'
                        + "    target_compatible_with = HOST_CONSTRAINTS,\n"
                        + ")\n"
                    ),
                },
            )
            os.makedirs(os.path.join(tmpdir, "tools/my_tool"), exist_ok=True)
            with open(
                os.path.join(tmpdir, "tools/my_tool/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/go:defs.bzl", "go_binary")\n\n'
                    + 'go_binary(name = "my_tool", srcs = ["main.go"])\n'
                )
            with open(
                os.path.join(tmpdir, "src/unbacked/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "u",\n'
                    + '    srcs = ["lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, "src/piggyback/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "p",\n'
                    + '    srcs = ["lib.rs"],\n'
                    + '    crate_features = ["base_feat", "extra_feat"],\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n"
                )
            findings = _run_minimality(
                tmpdir,
                base,
                "tools/my_tool",
                [
                    "tools/my_tool/BUILD.bazel",
                    "src/unbacked/BUILD.bazel",
                    "src/piggyback/BUILD.bazel",
                ],
            )
            oos_files = {
                f["file"]
                for f in findings
                if f["category"] == "out_of_scope_package_modified"
            }
            self.assertIn("src/unbacked/BUILD.bazel", oos_files)
            self.assertIn("src/piggyback/BUILD.bazel", oos_files)


class MachineryExtraSquashedLessonsTest(unittest.TestCase):
    def _run_script(
        self,
        script_name: str,
        repo: str,
        base_sha: str,
        target_dir: str,
        changed_files: list[str],
    ) -> list[dict]:
        script_path = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), script_name
        )
        env = {
            k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")
        }
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
            ["bash", script_path],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        return json.loads(out.stdout)

    def test_cc_header_deps_untranslated_gn_commented_attr_and_orphaned_library_todo(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/my_crate/BUILD.gn": (
                        "# TODO(https://fxbug.dev/42000000): Re-enable tests in these environments.\n"
                        'rustc_library("my_crate") {\n'
                        "  with_unit_tests = true\n"
                        '  exclude_toolchain_tags = [ "asan" ]\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        "}\n"
                    ),
                    "src/my_crate/src/lib.rs": "pub fn f() {}\n",
                },
            )
            bz_rel = "src/my_crate/BUILD.bazel"
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library", "rustc_test")\n\n'
                    + "# TODO(https://fxbug.dev/42000000): Re-enable tests in these environments.\n"
                    + "rustc_library(\n"
                    + '    name = "my_crate",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    # configs += ["//build/config/rust:foo"]\n'
                    + '    # build_flags = ["//build/config/rust:bar"],\n'
                    + ")\n\n"
                    + "# TODO(https://fxbug.dev/42000000): Re-enable tests in these environments.\n"
                    + "rustc_test(\n"
                    + '    name = "my_crate_test",\n'
                    + '    crate = ":my_crate",\n'
                    + '    tags = ["exclude_toolchain_tags=asan"],\n'
                    + ")\n"
                )
            findings = self._run_script(
                "cc_header_deps_and_annotations.sh",
                tmpdir,
                base,
                "src/my_crate",
                [bz_rel],
            )
            cats = [f["category"] for f in findings]
            self.assertIn("orphaned_test_env_todo_on_library", cats)
            self.assertIn("untranslated_gn_commented_attr", cats)
            comment_hits = [
                f
                for f in findings
                if f["category"] == "untranslated_gn_commented_attr"
            ]
            self.assertEqual(len(comment_hits), 1)
            self.assertIn("configs +=", comment_hits[0]["message"])

    def test_transitive_legacy_host_constraints_macro_subtarget_and_atom_lists(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "sdk/atom_lists.bzl": (
                        'HOST_TOOLS = [\n    "//tools/my_atom_tool:my_atom_tool",\n]\n'
                    ),
                    "src/legacy_dep/BUILD.bazel": (
                        HEADER
                        + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
                        + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                        + "rustc_library(\n"
                        + '    name = "legacy_dep",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + "    target_compatible_with = HOST_CONSTRAINTS,\n"
                        + ")\n"
                    ),
                    "src/plugin/BUILD.bazel": (
                        HEADER
                        + 'load("//src/developer/ffx/build:ffx_plugin.bzl", "ffx_plugin")\n\n'
                        + "ffx_plugin(\n"
                        + '    name = "ffx_my_plugin",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + '    deps = ["//src/legacy_dep:legacy_dep"],\n'
                        + ")\n"
                    ),
                    "tools/my_atom_tool/BUILD.gn": (
                        'rustc_binary("my_atom_tool") {\n  sources = [ "src/main.rs" ]\n}\n'
                    ),
                },
            )
            bz_rel = "tools/my_atom_tool/BUILD.bazel"
            os.makedirs(
                os.path.dirname(os.path.join(tmpdir, bz_rel)), exist_ok=True
            )
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_binary")\n'
                    + 'load("//platforms/host:constraints.bzl", "HOST_OS_CONSTRAINTS")\n\n'
                    + "rustc_binary(\n"
                    + '    name = "my_atom_tool",\n'
                    + '    srcs = ["src/main.rs"],\n'
                    + '    deps = ["//src/plugin:ffx_my_plugin_suite"],\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n"
                )
            findings = _run_minimality(
                tmpdir, base, "tools/my_atom_tool", [bz_rel]
            )
            tlhc = [
                f
                for f in findings
                if f["category"] == "transitive_legacy_host_constraints"
            ]
            self.assertEqual(len(tlhc), 1)
            self.assertIn("//src/legacy_dep:legacy_dep", tlhc[0]["message"])

    def test_unused_test_data_ffx_plugin_helper_and_select_vs_unused_sibling(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/helper_plugin/BUILD.gn": (
                        'host_test_data("used_data") {\n'
                        '  sources = [ "testdata/used.json" ]\n'
                        '  outputs = [ "$root_out_dir/test_data/helper/used/used.json" ]\n'
                        "}\n"
                        'host_test_data("unused_sibling") {\n'
                        '  sources = [ "testdata/unused.json" ]\n'
                        '  outputs = [ "$root_out_dir/test_data/helper/unused/unused.json" ]\n'
                        "}\n"
                    ),
                    "src/helper_plugin/BUILD.bazel": (
                        HEADER
                        + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_test_data_files")\n'
                        + 'load("//src/developer/ffx/build:ffx_plugin.bzl", "ffx_plugin")\n\n'
                        + "host_test_data_files(\n"
                        + '    name = "used_data",\n'
                        + '    srcs = ["testdata/used.json"],\n'
                        + '    dest_dir = "test_data/repo_fixtures/empty-repo",\n'
                        + ")\n\n"
                        + "host_test_data_files(\n"
                        + '    name = "unused_sibling",\n'
                        + '    srcs = ["testdata/unused.json"],\n'
                        + '    dest_dir = "test_data/repo_fixtures/pm-repo",\n'
                        + ")\n\n"
                        + "ffx_plugin(\n"
                        + '    name = "ffx_helper",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + ")\n"
                    ),
                    "src/helper_plugin/src/lib.rs": (
                        "pub fn load_used_fixture() -> &'static str {\n"
                        '    "test_data/repo_fixtures/empty-repo/used.json"\n'
                        "}\n"
                    ),
                    "src/caller_test/src/main.rs": (
                        "#[test]\n"
                        "fn smoke() {\n"
                        "    let _ = ffx_helper::load_used_fixture();\n"
                        "}\n"
                    ),
                },
            )
            bz_rel = "src/caller_test/BUILD.bazel"
            os.makedirs(
                os.path.dirname(os.path.join(tmpdir, bz_rel)), exist_ok=True
            )
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_rustc_test")\n\n'
                    + "host_rustc_test(\n"
                    + '    name = "caller_test",\n'
                    + '    srcs = ["src/main.rs"],\n'
                    + "    deps = select({\n"
                    + '        "//conditions:default": ["//src/helper_plugin:ffx_helper_suite"],\n'
                    + "    }),\n"
                    + "    test_data = [\n"
                    + '        "//src/helper_plugin:used_data",\n'
                    + '        "//src/helper_plugin:unused_sibling",\n'
                    + "    ],\n"
                    + ")\n"
                )
            findings = self._run_script(
                "test_data_attribute.sh",
                tmpdir,
                base,
                "src/caller_test",
                [bz_rel],
            )
            unused = [
                f for f in findings if f["category"] == "unused_test_data"
            ]
            self.assertEqual(len(unused), 1)
            self.assertIn("unused_sibling", unused[0]["message"])
            self.assertNotIn("used_data", unused[0]["message"])

    def test_stacked_prerequisite_change_base_and_cross_host_depends_on(self):
        import sys

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        if checks_dir not in sys.path:
            sys.path.insert(0, checks_dir)
        import planter_check_lib as pcl

        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base)
            os.makedirs(
                os.path.join(tmpdir, "build/bazel/rules"), exist_ok=True
            )
            with open(
                os.path.join(tmpdir, "build/bazel/rules/helper.bzl"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write("HELPER = True\n")
            _git(tmpdir, "add", "build/bazel/rules/helper.bzl")
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[bazel][rules] Add helper\n\nChange-Id: I2222222222222222222222222222222222222222",
            )
            prereq_sha = _git(tmpdir, "rev-parse", "HEAD")

            # Uncommitted target migration stacked on top of prerequisite HEAD:
            os.makedirs(os.path.join(tmpdir, "src/leaf"), exist_ok=True)
            with open(
                os.path.join(tmpdir, "src/leaf/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'package(default_visibility = ["//visibility:public"])\n'
                )
            from unittest import mock

            with mock.patch.dict(
                os.environ,
                {
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_CHANGED_FILES": (
                        "build/bazel/rules/helper.bzl\nsrc/leaf/BUILD.bazel"
                    ),
                },
                clear=False,
            ):
                os.environ.pop("PLANTER_RESOLVED_CHANGE_BASE", None)
                resolved = pcl.resolve_change_base(
                    tmpdir, change_base=None, target_dirs="src/leaf"
                )
                self.assertEqual(resolved, "HEAD")
                self.assertEqual(
                    pcl.changed_files(
                        tmpdir, change_base=None, target_dirs="src/leaf"
                    ),
                    ["src/leaf/BUILD.bazel"],
                )

            # Commit target migration with cross-host Depends-on: other-host:I...
            _git(tmpdir, "add", "src/leaf/BUILD.bazel")
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[bazel_migration] //src/leaf\n\n"
                "Depends-on: other-host:I1111111111111111111111111111111111111111\n"
                "Depends-on: I2222222222222222222222222222222222222222\n"
                "Change-Id: I3333333333333333333333333333333333333333",
            )
            cq_findings = self._run_script(
                "cq_reachability.sh",
                tmpdir,
                prereq_sha,
                "src/leaf",
                ["src/leaf/BUILD.bazel"],
            )
            unstacked = [
                f
                for f in cq_findings
                if "unstacked" in f["category"] or "depends_on" in f["category"]
            ]
            self.assertEqual(unstacked, [])

    def test_round1_gn_foreach_gni_and_unresolved_loop_preservation(self):
        import ast
        import re
        import sys

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.join(os.path.dirname(checks_dir), "tools")
        if checks_dir not in sys.path:
            sys.path.insert(0, checks_dir)
        import planter_check_lib as pcl

        with open(
            os.path.join(tools_dir, "inspect_gn_deps.sh"), encoding="utf-8"
        ) as sf:
            ig_txt = sf.read()
        m = re.search(
            r"python3[^\n]*<<'PYEOF'\n(.*?)\nPYEOF", ig_txt, flags=re.S
        )
        assert m is not None
        tree = ast.parse(m.group(1), filename="inspect_gn_deps.sh")
        wanted = {
            "DEP_LIST_ATTRS",
            "expand_gn_foreach_and_vars",
            "extract_gn_deps_and_inputs",
        }
        selected = [
            n
            for n in tree.body
            if (isinstance(n, ast.FunctionDef) and n.name in wanted)
            or (
                isinstance(n, ast.Assign)
                and any(
                    isinstance(t, ast.Name) and t.id in wanted
                    for t in n.targets
                )
            )
        ]

        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/common/defs.gni": 'gni_variants = [ "v1" ]\n',
                    "src/ext/v1/BUILD.gn": (
                        'config("cfg") { ldflags = [ "-lm" ] }\n'
                        'rustc_library("v1") {\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        '  public_configs = [ ":cfg" ]\n'
                        "}\n"
                    ),
                    "src/ext/blocker/BUILD.gn": (
                        'config("bcfg") { ldflags = [ "-ldl" ] }\n'
                        'rustc_library("blocker") {\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        '  public_configs = [ ":bcfg" ]\n'
                        "}\n"
                    ),
                    "src/pkg/BUILD.gn": (
                        'import("//src/common/defs.gni")\n'
                        "foreach(v, gni_variants) {\n"
                        '  rustc_library("lib_${v}") {\n'
                        '    sources = [ "src/lib.rs" ]\n'
                        '    deps = [ "//src/ext/${v}:${v}" ]\n'
                        "  }\n"
                        "}\n"
                        "foreach(u, unknown_arg_list) {\n"
                        '  rustc_library("fixed_lib") {\n'
                        '    sources = [ "src/lib.rs" ]\n'
                        '    deps = [ "//src/ext/blocker:blocker" ]\n'
                        '    inputs = [ "//src/ext/blocker/data.json" ]\n'
                        "  }\n"
                        "}\n"
                        'rustc_library("first") {\n'
                        '  protos = [ "fuchsia.io" ]\n'
                        "  deps = []\n"
                        "  foreach(p, protos) {\n"
                        '    deps += [ "//sdk/fidl/$p:${p}_rust" ]\n'
                        "  }\n"
                        "}\n"
                        'rustc_library("second") {\n'
                        '  protos = [ "fuchsia.net" ]\n'
                        "  deps = []\n"
                        "  foreach(p, protos) {\n"
                        '    deps += [ "//sdk/fidl/$p:${p}_rust" ]\n'
                        "  }\n"
                        "}\n"
                    ),
                },
            )
            ns = {"os": os, "re": re, "pcl": pcl, "workdir": tmpdir}
            exec(
                compile(
                    ast.Module(body=selected, type_ignores=[]),
                    "inspect_gn_deps.sh",
                    "exec",
                ),
                ns,
            )
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.gn"), encoding="utf-8"
            ) as gf:
                gn_src = gf.read()
            deps, inputs = ns["extract_gn_deps_and_inputs"](gn_src, "src/pkg")
            self.assertEqual(
                deps,
                [
                    "//src/ext/v1:v1",
                    "//src/ext/blocker:blocker",
                    "//sdk/fidl/fuchsia.io:fuchsia.io_rust",
                    "//sdk/fidl/fuchsia.net:fuchsia.net_rust",
                ],
            )
            self.assertEqual(inputs, ["//src/ext/blocker/data.json"])

            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as gf:
                gf.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("lib_v1") { sources = [ "src/lib.rs" ] }\n'
                    'rustc_library("fixed_lib") { sources = [ "src/lib.rs" ] }\n'
                    'rustc_library("first") { sources = [ "src/lib.rs" ] }\n'
                    'rustc_library("second") { sources = [ "src/lib.rs" ] }\n'
                )
            parity_findings = self._run_script(
                "gn_dep_parity.sh",
                tmpdir,
                base,
                "src/pkg",
                ["src/pkg/BUILD.gn"],
            )
            dropped_msgs = [
                f["message"]
                for f in parity_findings
                if f["category"] == "dep_link_settings_dropped"
            ]
            self.assertTrue(
                any(
                    "lib_v1" in m and "//src/ext/v1:v1" in m
                    for m in dropped_msgs
                ),
                dropped_msgs,
            )
            self.assertTrue(
                any(
                    "fixed_lib" in m and "//src/ext/blocker:blocker" in m
                    for m in dropped_msgs
                ),
                dropped_msgs,
            )

    def test_round1_stacked_head_resolution_in_build_only_scope_and_test_data(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "tools/devshell/foo/BUILD.gn": (
                        'host_test_data("td") {\n'
                        '  sources = [ "kept.txt", "omitted.txt" ]\n'
                        '  outputs = [ "$root_out_dir/test_data/foo/{{source_file_part}}" ]\n'
                        "}\n"
                    ),
                    "tools/devshell/foo/tool.py": "x = 1\n",
                    "tools/devshell/foo/kept.txt": "ok\n",
                    "tools/devshell/foo/omitted.txt": "ok\n",
                },
            )
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base)
            with open(
                os.path.join(tmpdir, "tools/devshell/foo/tool.py"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write("x = 2\n")
            with open(
                os.path.join(tmpdir, "tools/devshell/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_test_data_files")\n\n'
                    + "host_test_data_files(\n"
                    + '    name = "td",\n'
                    + '    srcs = ["kept.txt"],\n'
                    + '    dest_dir = "test_data/foo",\n'
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, "tools/devshell/foo/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                )
            _git(
                tmpdir,
                "add",
                "tools/devshell/foo/tool.py",
                "tools/devshell/foo/BUILD.bazel",
                "tools/devshell/foo/BUILD.gn",
            )
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[bazel] Migrate devshell helper\n\nChange-Id: I4444444444444444444444444444444444444444",
            )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_CHANGE_BASE": "HEAD",
                    "PLANTER_TARGET_DIR": "tools/devshell/foo",
                    "PLANTER_TARGET_DIRS": "tools/devshell/foo",
                    "PYTHONDONTWRITEBYTECODE": "1",
                }
            )
            checks_dir = os.path.dirname(os.path.abspath(__file__))
            bos_out = subprocess.check_output(
                ["bash", os.path.join(checks_dir, "build_only_scope.sh")],
                env=env,
                text=True,
            )
            bos_findings = json.loads(bos_out)
            self.assertTrue(
                any(
                    f["category"] == "non_build_file_modified"
                    and f["file"] == "tools/devshell/foo/tool.py"
                    for f in bos_findings
                ),
                bos_findings,
            )
            tda_out = subprocess.check_output(
                ["bash", os.path.join(checks_dir, "test_data_attribute.sh")],
                env=env,
                text=True,
            )
            tda_findings = json.loads(tda_out)
            self.assertTrue(
                any(
                    "omitted_host_test_data" in f["category"]
                    for f in tda_findings
                ),
                tda_findings,
            )

    def test_round2_stacked_prereq_pinned_base_and_stale_changed_files(self):
        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.join(os.path.dirname(checks_dir), "tools")

        def clean_env(**overrides):
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env["PYTHONDONTWRITEBYTECODE"] = "1"
            env.update(overrides)
            return env

        with tempfile.TemporaryDirectory() as tmpdir:
            base_a = _init_repo(
                tmpdir,
                {
                    "src/prereq/helper.py": "X = 1\n",
                    "src/prereq/prereq_tool": "bin\n",
                    "src/prereq/BUILD.gn": (
                        'host_test_data("prereq_td") {\n'
                        '  sources = [ "prereq_tool" ]\n'
                        '  outputs = [ "$root_out_dir/test_data/shared/prereq_tool" ]\n'
                        "}\n"
                    ),
                    "src/leaf/leaf_data.txt": "leaf\n",
                    "src/leaf/BUILD.gn": (
                        'host_test_data("leaf_td") {\n'
                        '  sources = [ "leaf_data.txt" ]\n'
                        '  outputs = [ "$root_out_dir/test_data/shared/leaf_data.txt" ]\n'
                        "}\n"
                    ),
                },
            )
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base_a)

            with open(
                os.path.join(tmpdir, "src/prereq/helper.py"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write("X = 2\n")
            with open(
                os.path.join(tmpdir, "src/prereq/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + "# Not staged yet: prereq_tool needs //src/other:producer (pending migration prerequisite).\n"
                    + 'package(default_visibility = ["//visibility:public"])\n'
                )
            with open(
                os.path.join(tmpdir, "src/prereq/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                )
            _git(tmpdir, "add", ".")
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[tools] //src/prereq: migrate prereq\n\n"
                "Bazel-Migration-Target: //src/prereq\n"
                "Change-Id: I1111111111111111111111111111111111111111",
            )

            with open(
                os.path.join(tmpdir, "src/leaf/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host_tests:defs.bzl", "host_test_data_files")\n\n'
                    + "host_test_data_files(\n"
                    + '    name = "leaf_td",\n'
                    + '    srcs = ["leaf_data.txt"],\n'
                    + '    dest_dir = "test_data/shared",\n'
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, "src/leaf/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                )

            stale_changed = "\n".join(
                [
                    "src/prereq/helper.py",
                    "src/prereq/BUILD.bazel",
                    "src/prereq/BUILD.gn",
                    "src/leaf/BUILD.bazel",
                    "src/leaf/BUILD.gn",
                ]
            )
            rc_out = subprocess.run(
                [
                    "bash",
                    os.path.join(tools_dir, "run_checks.sh"),
                    "--only=bazel_minimality,build_only_scope,test_data_attribute",
                    "src/leaf",
                ],
                env=clean_env(
                    PLANTER_WORKDIR=tmpdir,
                    PLANTER_TARGET_DIR="src/leaf",
                    PLANTER_TARGET_DIRS="src/leaf",
                    PLANTER_CHANGE_BASE=base_a,
                    PLANTER_CHANGED_FILES=stale_changed,
                    PLANTER_SKIP_BUILD="1",
                    PLANTER_SKIP_FETCH="1",
                ),
                capture_output=True,
                text=True,
            )
            self.assertEqual(
                rc_out.returncode, 0, rc_out.stdout + rc_out.stderr
            )

            bos_findings = json.loads(
                subprocess.check_output(
                    ["bash", os.path.join(checks_dir, "build_only_scope.sh")],
                    env=clean_env(
                        PLANTER_WORKDIR=tmpdir,
                        PLANTER_TARGET_DIR="src/leaf",
                        PLANTER_TARGET_DIRS="src/leaf",
                        PLANTER_CHANGE_BASE=base_a,
                    ),
                    text=True,
                )
            )
            self.assertEqual(bos_findings, [])

            tda_findings = json.loads(
                subprocess.check_output(
                    [
                        "bash",
                        os.path.join(checks_dir, "test_data_attribute.sh"),
                    ],
                    env=clean_env(
                        PLANTER_WORKDIR=tmpdir,
                        PLANTER_TARGET_DIR="src/leaf",
                        PLANTER_TARGET_DIRS="src/leaf",
                        PLANTER_CHANGE_BASE=base_a,
                        PLANTER_CHANGED_FILES=stale_changed,
                    ),
                    text=True,
                )
            )
            self.assertEqual(tda_findings, [])

            # Also verify committed leaf on top of stacked prerequisite with pinned base_a.
            _git(tmpdir, "add", "src/leaf/BUILD.bazel", "src/leaf/BUILD.gn")
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[bazel_migration] //src/leaf\n\n"
                "Depends-on: I1111111111111111111111111111111111111111\n"
                "Change-Id: I2222222222222222222222222222222222222222",
            )
            bos_committed = json.loads(
                subprocess.check_output(
                    ["bash", os.path.join(checks_dir, "build_only_scope.sh")],
                    env=clean_env(
                        PLANTER_WORKDIR=tmpdir,
                        PLANTER_TARGET_DIR="src/leaf",
                        PLANTER_TARGET_DIRS="src/leaf",
                        PLANTER_CHANGE_BASE=base_a,
                    ),
                    text=True,
                )
            )
            self.assertEqual(bos_committed, [])
            tda_committed = json.loads(
                subprocess.check_output(
                    [
                        "bash",
                        os.path.join(checks_dir, "test_data_attribute.sh"),
                    ],
                    env=clean_env(
                        PLANTER_WORKDIR=tmpdir,
                        PLANTER_TARGET_DIR="src/leaf",
                        PLANTER_TARGET_DIRS="src/leaf",
                        PLANTER_CHANGE_BASE=base_a,
                        PLANTER_CHANGED_FILES=stale_changed,
                    ),
                    text=True,
                )
            )
            self.assertEqual(tda_committed, [])

    def test_round3_bare_foreach_list_ident_and_build_secondary_task_head(self):
        import ast
        import re
        import sys
        from unittest import mock

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.join(os.path.dirname(checks_dir), "tools")
        if checks_dir not in sys.path:
            sys.path.insert(0, checks_dir)
        import planter_check_lib as pcl

        def load_funcs(path, wanted, extra_ns):
            with open(path, encoding="utf-8") as sf:
                txt = sf.read()
            m = re.search(
                r"python3[^\n]*<<'PYEOF'\n(.*?)\nPYEOF", txt, flags=re.S
            )
            assert m is not None
            tree = ast.parse(m.group(1), filename=os.path.basename(path))
            selected = [
                n
                for n in tree.body
                if (isinstance(n, ast.FunctionDef) and n.name in wanted)
                or (
                    isinstance(n, ast.Assign)
                    and any(
                        isinstance(t, ast.Name) and t.id in wanted
                        for t in n.targets
                    )
                )
            ]
            ns = dict(extra_ns)
            exec(
                compile(
                    ast.Module(body=selected, type_ignores=[]),
                    os.path.basename(path),
                    "exec",
                ),
                ns,
            )
            return ns

        # Bare loop-variable list elements inside foreach bodies.
        cases = [
            ("deps += [ t ]", "t", "//src/a:a", 'deps += [ "//src/a:a" ]'),
            (
                'deps += [ t, "$t", foo.t, tt, t(x) ]',
                "t",
                "//a",
                'deps += [ "//a", "$t", foo.t, tt, t(x) ]',
            ),
            ("t = [ x ]", "t", "//a", "t = [ x ]"),
            ("deps = [ v ]", "t", "//a", "deps = [ v ]"),
        ]
        dp_ns = load_funcs(
            os.path.join(checks_dir, "gn_dep_parity.sh"),
            {"_quote_bare_gn_list_ident"},
            {"re": re},
        )
        for code, name, val, want in cases:
            self.assertEqual(
                pcl.quote_bare_gn_list_ident(code, name, val), want
            )
            self.assertEqual(
                dp_ns["_quote_bare_gn_list_ident"](code, name, val), want
            )

        ig_ns = load_funcs(
            os.path.join(tools_dir, "inspect_gn_deps.sh"),
            {
                "DEP_LIST_ATTRS",
                "expand_gn_foreach_and_vars",
                "extract_gn_deps_and_inputs",
            },
            {"os": os, "re": re, "pcl": pcl, "workdir": "/nonexistent"},
        )
        bare = (
            '_tools = [ "//src/a:a", "//src/b:b" ]\n'
            'group("g") {\n'
            "  deps = []\n"
            "  foreach(t, _tools) {\n"
            "    deps += [ t ]\n"
            "  }\n"
            "}\n"
        )
        quoted = bare.replace("[ t ]", '[ "$t" ]')
        for src in (bare, quoted):
            deps, inputs = ig_ns["extract_gn_deps_and_inputs"](src, "src/pkg")
            self.assertEqual(deps, ["//src/a:a", "//src/b:b"])
            self.assertEqual(inputs, [])

        # A build/secondary/<dir> task's own //<dir> commit is not a stacked prerequisite.
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {"build/secondary/src/foo/BUILD.gn": 'group("foo") {}\n'},
            )
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base)
            os.makedirs(os.path.join(tmpdir, "src/foo"), exist_ok=True)
            _git(
                tmpdir,
                "mv",
                "build/secondary/src/foo/BUILD.gn",
                "src/foo/BUILD.gn",
            )
            with open(
                os.path.join(tmpdir, "src/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(HEADER)
            _git(tmpdir, "add", ".")
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[bazel_migration] //src/foo\n\n"
                "Bazel-Migration-Target: //src/foo:foo",
            )
            self.assertFalse(
                pcl.is_stacked_prerequisite_head(
                    "[bazel_migration] //src/foo",
                    "Bazel-Migration-Target: //src/foo:foo",
                    ["src/foo/BUILD.bazel", "src/foo/BUILD.gn"],
                    ["build/secondary/src/foo"],
                )
            )
            for pinned, want in (("HEAD", "HEAD~1"), (base, base)):
                env = {
                    k: v
                    for k, v in os.environ.items()
                    if not k.startswith("PLANTER_")
                }
                env["PLANTER_CHANGE_BASE"] = pinned
                with mock.patch.dict(os.environ, env, clear=True):
                    cb = pcl.resolve_change_base(
                        tmpdir, target_dirs=["build/secondary/src/foo"]
                    )
                    self.assertEqual(cb, want)
                    self.assertIn(
                        "src/foo/BUILD.bazel",
                        pcl.changed_files(
                            tmpdir, cb, target_dirs=["build/secondary/src/foo"]
                        ),
                    )

    def test_round4_overlay_base_in_bazel_minimality_and_cross_host_hint(self):
        import ast
        import re
        from unittest import mock

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        with open(
            os.path.join(checks_dir, "bazel_minimality.sh"), encoding="utf-8"
        ) as sf:
            body = re.search(
                r"<<'PYEOF'\n(.*?)\nPYEOF", sf.read(), flags=re.S
            ).group(1)
        wanted = {
            "_with_overlaid_target_dirs",
            "git_lines",
            "git_out",
            "is_ancestor",
            "normalize_git_rev",
            "_has_uncommitted_worktree_edits",
            "_is_stacked_prerequisite_head",
            "resolve_effective_change_base",
            "SHARED_BAZEL_RULE_PREFIXES",
        }
        selected = [
            n
            for n in ast.parse(body).body
            if (isinstance(n, ast.FunctionDef) and n.name in wanted)
            or (
                isinstance(n, ast.Assign)
                and any(
                    isinstance(t, ast.Name) and t.id in wanted
                    for t in n.targets
                )
            )
        ]
        x_cid = "I" + "a" * 40
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {"build/secondary/src/foo/BUILD.gn": 'group("foo") {}\n'},
            )
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base)
            os.makedirs(os.path.join(tmpdir, "src/foo"), exist_ok=True)
            _git(
                tmpdir,
                "mv",
                "build/secondary/src/foo/BUILD.gn",
                "src/foo/BUILD.gn",
            )
            with open(
                os.path.join(tmpdir, "src/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(HEADER)
            _git(tmpdir, "add", ".")
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[bazel_migration] //src/foo\n\n"
                "Bazel-Migration-Target: //src/foo:foo",
            )
            ns = {
                "os": os,
                "re": re,
                "subprocess": subprocess,
                "workdir": tmpdir,
                "target_dirs": ["build/secondary/src/foo"],
            }
            exec(
                compile(
                    ast.Module(body=selected, type_ignores=[]),
                    "bazel_minimality.sh",
                    "exec",
                ),
                ns,
            )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            with mock.patch.dict(os.environ, env, clear=True):
                self.assertEqual(
                    ns["resolve_effective_change_base"]("HEAD"), "HEAD~1"
                )

            # End-to-end bazel_minimality.sh under PLANTER_CHANGE_BASE=HEAD must
            # accept the overlaid <dir>/BUILD.bazel and <dir>/BUILD.gn in scope.
            e2e_env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            e2e_env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "build/secondary/src/foo",
                    "PLANTER_TARGET_DIRS": "build/secondary/src/foo",
                    "PLANTER_CHANGE_BASE": "HEAD",
                    "PLANTER_SKIP_FETCH": "1",
                }
            )
            bm_out = subprocess.run(
                ["bash", SCRIPT],
                env=e2e_env,
                capture_output=True,
                text=True,
                check=True,
            )
            bm_findings = json.loads(bm_out.stdout)
            self.assertEqual(
                [
                    f
                    for f in bm_findings
                    if f["category"] == "out_of_scope_package_modified"
                ],
                [],
                bm_findings,
            )

            # Cross-host Depends-on trailers keep their host prefix in the
            # malformed_depends_on_footer remediation hint, and cq_reachability.sh
            # expands build/secondary/<dir> to check the overlaid <dir> package.
            with open(
                os.path.join(tmpdir, "src/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/cc:defs.bzl", "cc_test")\n\n'
                    + 'cc_test(name = "unwired_foo_test")\n'
                )
            _git(tmpdir, "add", "src/foo/BUILD.bazel")
            _git(
                tmpdir,
                "commit",
                "-q",
                "--amend",
                "-m",
                "[bazel_migration] //src/foo\n\n"
                "Depends on: fxrev.dev/123\n\n"
                f"Depends-on: turquoise-internal:{x_cid}\n"
                "Change-Id: I" + "b" * 40,
            )
            cq_findings = self._run_script(
                "cq_reachability.sh",
                tmpdir,
                base,
                "build/secondary/src/foo",
                ["src/foo/BUILD.bazel", "src/foo/BUILD.gn"],
            )
            malformed = [
                f
                for f in cq_findings
                if f["category"] == "malformed_depends_on_footer"
            ]
            self.assertTrue(malformed, cq_findings)
            blob = json.dumps(malformed)
            self.assertIn(f"Depends-on: turquoise-internal:{x_cid}", blob)
            self.assertNotIn(f"`Depends-on: {x_cid}`", blob)
            self.assertTrue(
                any(
                    f["category"] == "orphaned_test_target" for f in cq_findings
                ),
                cq_findings,
            )

    def test_round5_overlay_inspect_lint_and_template_args(self):
        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.join(os.path.dirname(checks_dir), "tools")

        # 1. inspect_gn_deps.sh maps build/secondary/<dir> to overlaid <dir>
        # and expands in_scope with overlaid directories.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "build/secondary/third_party/mylib/BUILD.gn": (
                        'source_set("helper") {}\n'
                        'source_set("mylib") {\n'
                        "  deps = [\n"
                        '    "//third_party/mylib:helper",\n'
                        '    "//src/restricted:lib",\n'
                        '    "//third_party/peer:peer",\n'
                        "  ]\n"
                        "}\n"
                    ),
                    "third_party/mylib/BUILD.bazel": (
                        HEADER + 'cc_library(name = "mylib")\n'
                    ),
                    "third_party/BUILD.bazel": (
                        HEADER + 'test_suite(name = "tests", tests = [])\n'
                    ),
                    "src/restricted/BUILD.bazel": (
                        HEADER
                        + "cc_library(\n"
                        + '    name = "lib",\n'
                        + '    visibility = ["//src/restricted:__pkg__"],\n'
                        + ")\n"
                    ),
                },
            )
            ig_env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            ig_env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIRS": (
                        "build/secondary/third_party/mylib,"
                        "build/secondary/third_party/peer"
                    ),
                }
            )
            ig_out = subprocess.run(
                [
                    "bash",
                    os.path.join(tools_dir, "inspect_gn_deps.sh"),
                    "build/secondary/third_party/mylib",
                ],
                env=ig_env,
                capture_output=True,
                text=True,
                check=True,
            )
            report = json.loads(ig_out.stdout)
            self.assertEqual(report["target_package"], "//third_party/mylib")
            self.assertTrue(report["has_build_gn"])
            self.assertTrue(report["has_build_bazel"])
            self.assertEqual(report["target_name_mismatches"], [])
            self.assertEqual(
                report["missing_in_scope_bazel_packages"],
                ["//third_party/peer"],
            )
            self.assertEqual(report["missing_external_bazel_packages"], [])
            self.assertEqual(
                report["visibility_edits_needed"][0]["add_visibility"],
                "//third_party/mylib:__pkg__",
            )
            self.assertEqual(
                report["nearest_ancestor_test_suite"]["bazel_build"],
                "third_party/BUILD.bazel",
            )

        # 2. machinery_script_lint.sh restricts module exports to top-level
        # symbols and tracks globals read inside lambdas / inner helper defs.
        lint_script = os.path.join(checks_dir, "machinery_script_lint.sh")
        with tempfile.TemporaryDirectory() as tmpdir:
            sample = os.path.join(tmpdir, "sample.sh")
            with open(sample, "w", encoding="utf-8") as sf:
                sf.write(
                    "#!/usr/bin/env bash\n"
                    "python3 - <<'PYEOF'\n"
                    "import planter_check_lib as pcl\n\n"
                    "def run_lambda(xs):\n"
                    "    return sorted(xs, key=lambda k: LATE_MAP[k])\n\n"
                    "def run_nested(n):\n"
                    "    def walk(v):\n"
                    "        return walk(v - 1) if v > 0 else LATE_VAL\n"
                    "    return walk(n)\n\n"
                    "_ = pcl.tdirs\n"
                    "run_lambda([])\n"
                    "run_nested(0)\n"
                    "LATE_MAP = {}\n"
                    "LATE_VAL = 1\n"
                    "PYEOF\n"
                )
            lint_env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            lint_env["PLANTER_WORKDIR"] = tmpdir
            lint_out = subprocess.run(
                ["bash", lint_script, sample],
                env=lint_env,
                capture_output=True,
                text=True,
                check=True,
            )
            lint_msgs = " ".join(
                f["message"] for f in json.loads(lint_out.stdout)
            )
            self.assertIn("pcl.tdirs", lint_msgs)
            self.assertIn("LATE_MAP", lint_msgs)
            self.assertIn("LATE_VAL", lint_msgs)

        # 3. Removal of dead PLANTER_RAW_CHANGE_BASE in tools/run_checks.sh.
        with open(
            os.path.join(tools_dir, "run_checks.sh"), encoding="utf-8"
        ) as rcf:
            rc_src = rcf.read()
        self.assertNotIn("PLANTER_RAW_CHANGE_BASE", rc_src)

    def test_round7_overlay_inputs_commented_attrs_and_branched_gn_vars(
        self,
    ) -> None:
        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.join(os.path.dirname(checks_dir), "tools")

        # 1. R3-1: migration_sanity.sh maps build/secondary/<dir> to <dir>/BUILD.bazel.
        ms_script = os.path.join(checks_dir, "migration_sanity.sh")
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "build/secondary/third_party/mylib/BUILD.gn": (
                        HEADER + 'source_set("mylib") {}\n'
                    ),
                },
            )
            os.makedirs(
                os.path.join(tmpdir, "third_party/mylib"), exist_ok=True
            )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PYTHONDONTWRITEBYTECODE": "1",
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "build/secondary/third_party/mylib",
                    "PLANTER_TARGET_DIRS": "build/secondary/third_party/mylib",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_CHANGED_FILES": "",
                }
            )
            pre_findings = json.loads(
                subprocess.run(
                    ["bash", ms_script],
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            missing_pre = [
                f
                for f in pre_findings
                if f["category"] == "missing_target_bazel_file"
            ]
            self.assertEqual(len(missing_pre), 1)
            self.assertIn(
                "'third_party/mylib/BUILD.bazel'", missing_pre[0]["message"]
            )
            self.assertNotIn(
                "build/secondary/third_party/mylib/BUILD.bazel",
                missing_pre[0]["remediation"],
            )

            with open(
                os.path.join(tmpdir, "third_party/mylib/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as bzf:
                bzf.write(HEADER + 'cc_library(name = "mylib")\n')
            env["PLANTER_CHANGED_FILES"] = "third_party/mylib/BUILD.bazel"
            post_findings = json.loads(
                subprocess.run(
                    ["bash", ms_script],
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(
                [
                    f
                    for f in post_findings
                    if f["category"] == "missing_target_bazel_file"
                ],
                [],
            )

        # 2. R3-2: test_data_attribute.sh expands build/secondary/<dir> in target_dirs.
        tda_script = os.path.join(checks_dir, "test_data_attribute.sh")
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "build/secondary/src/caller/BUILD.gn": (
                        HEADER + 'rustc_test("caller") {}\n'
                    ),
                    "src/caller/src/main.rs": "#[test]\nfn t() {}\n",
                    "src/helper/BUILD.bazel": (
                        HEADER
                        + "host_test_data_files(\n"
                        + '    name = "unused_data",\n'
                        + '    srcs = ["a.json"],\n'
                        + '    dest_dir = "test_data/xyz_fixture",\n'
                        + ")\n"
                    ),
                },
            )
            with open(
                os.path.join(tmpdir, "src/caller/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as bzf:
                bzf.write(
                    HEADER
                    + "host_rustc_test(\n"
                    + '    name = "caller",\n'
                    + '    srcs = ["src/main.rs"],\n'
                    + '    test_data = ["//src/helper:unused_data"],\n'
                    + ")\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PYTHONDONTWRITEBYTECODE": "1",
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "build/secondary/src/caller",
                    "PLANTER_TARGET_DIRS": "build/secondary/src/caller",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_CHANGED_FILES": "src/caller/BUILD.bazel",
                }
            )
            tda_cats = [
                f["category"]
                for f in json.loads(
                    subprocess.run(
                        ["bash", tda_script],
                        env=env,
                        check=True,
                        capture_output=True,
                        text=True,
                    ).stdout
                )
            ]
            self.assertIn("unused_test_data", tda_cats)

        # 3. R3-3: cc_header_deps_and_annotations.sh hints for public_configs & list +=.
        cc_script = os.path.join(
            checks_dir, "cc_header_deps_and_annotations.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {"src/pkg/BUILD.gn": HEADER + 'source_set("pkg") {}\n'},
            )
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as bzf:
                bzf.write(
                    HEADER
                    + "cc_library(\n"
                    + '    name = "pkg",\n'
                    + '    srcs = ["a.cc"],\n'
                    + '    # public_configs = [":cfg"]\n'
                    + '    # sources += ["b.cc"]\n'
                    + ")\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PYTHONDONTWRITEBYTECODE": "1",
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/pkg",
                    "PLANTER_CHANGE_BASE": base,
                }
            )
            rems = [
                f["remediation"]
                for f in json.loads(
                    subprocess.run(
                        ["bash", cc_script],
                        env=env,
                        check=True,
                        capture_output=True,
                        text=True,
                    ).stdout
                )
                if f["category"] == "untranslated_gn_commented_attr"
            ]
            self.assertEqual(len(rems), 2)
            self.assertIn("`# build_flags = [...],`", rems[0])
            self.assertNotIn("`# public_configs = ...,`", rems[0])
            self.assertIn(
                "Remove dead commented-out GN `sources` syntax", rems[1]
            )

        # 5. R3-5: inspect_gn_deps.sh same-package inputs and build/secondary boundary.
        igd_script = os.path.join(tools_dir, "inspect_gn_deps.sh")
        with tempfile.TemporaryDirectory() as tmpdir:
            for d_sub in (
                "src/pkg/testdata",
                "build/secondary/third_party/peer",
                "third_party/peer",
            ):
                os.makedirs(os.path.join(tmpdir, d_sub), exist_ok=True)
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as gnf:
                gnf.write(
                    HEADER
                    + 'rustc_test("pkg_test") {\n'
                    + "  inputs = [\n"
                    + '    "//src/pkg/testdata/sample.wav",\n'
                    + '    "//third_party/peer/schema.json",\n'
                    + "  ]\n"
                    + "}\n"
                )
            open(
                os.path.join(tmpdir, "src/pkg/testdata/sample.wav"),
                "w",
                encoding="utf-8",
            ).close()
            with open(
                os.path.join(tmpdir, "third_party/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as bzf:
                bzf.write(
                    HEADER
                    + 'package(default_visibility = ["//visibility:public"])\n'
                )
            with open(
                os.path.join(
                    tmpdir, "build/secondary/third_party/peer/BUILD.gn"
                ),
                "w",
                encoding="utf-8",
            ) as gnf:
                gnf.write(HEADER + 'source_set("peer") {}\n')
            with open(
                os.path.join(tmpdir, "third_party/peer/schema.json"),
                "w",
                encoding="utf-8",
            ) as sf:
                sf.write("{}\n")
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {"PYTHONDONTWRITEBYTECODE": "1", "PLANTER_WORKDIR": tmpdir}
            )
            pre_inp = json.loads(
                subprocess.run(
                    ["bash", igd_script, "src/pkg"],
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )["inputs_status"]
            self.assertEqual(
                pre_inp,
                [
                    {
                        "gn_input": "//src/pkg/testdata/sample.wav",
                        "status": "ready",
                        "bazel_label": ":testdata/sample.wav",
                    },
                    {
                        "gn_input": "//third_party/peer/schema.json",
                        "status": "missing_owner_bazel_build",
                        "owner_package": "//third_party/peer",
                    },
                ],
            )

        # 6. R3-6: speculative_unit_test.sh reads build/secondary/<d>/BUILD.gn.
        sut_script = os.path.join(checks_dir, "speculative_unit_test.sh")
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "build/secondary/third_party/mycrate/BUILD.gn": (
                        HEADER
                        + 'rustc_library("c") {\n  sources = [ "src/lib.rs" ]\n}\n'
                    ),
                    "third_party/mycrate/src/lib.rs": "pub fn f() -> u32 { 1 }\n",
                },
            )
            with open(
                os.path.join(tmpdir, "third_party/mycrate/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as bzf:
                bzf.write(
                    HEADER
                    + "rustc_library(\n"
                    + '    name = "c",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + "    with_unit_tests = True,\n"
                    + ")\n"
                    + "wrap_host_rust_test(\n"
                    + '    name = "c_host_test",\n'
                    + '    binary_name = "c_test",\n'
                    + ")\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PYTHONDONTWRITEBYTECODE": "1",
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "build/secondary/third_party/mycrate",
                    "PLANTER_TARGET_DIRS": "build/secondary/third_party/mycrate",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_CHANGED_FILES": "third_party/mycrate/BUILD.bazel",
                }
            )
            sut_cats = [
                f["category"]
                for f in json.loads(
                    subprocess.run(
                        ["bash", sut_script],
                        env=env,
                        check=True,
                        capture_output=True,
                        text=True,
                    ).stdout
                )
            ]
            self.assertIn("speculative_empty_unit_test", sut_cats)
            self.assertIn("speculative_host_unit_test", sut_cats)

    def test_round8_waivers_rust_dep_scan_and_empty_diff_pass(self):
        checks_dir = os.path.dirname(os.path.abspath(__file__))
        machinery_dir = os.path.dirname(checks_dir)
        tools_dir = os.path.join(machinery_dir, "tools")
        rc_script = os.path.join(tools_dir, "run_checks.sh")
        gdp_script = os.path.join(checks_dir, "gn_dep_parity.sh")

        # 2. R4-2: empty_migration_diff RESULT: PASS path strips build/secondary/.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "src/unmig/BUILD.gn": HEADER + 'source_set("u") {}\n',
                    "build/secondary/third_party/mylib/BUILD.gn": (
                        HEADER
                        + 'source_set("mylib") { deps = [ "//src/unmig:u" ] }\n'
                    ),
                },
            )
            os.makedirs(
                os.path.join(tmpdir, "third_party/mylib"), exist_ok=True
            )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PYTHONDONTWRITEBYTECODE": "1",
                    "PLANTER_PROJECT_DIR": machinery_dir,
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "build/secondary/third_party/mylib",
                    "PLANTER_TARGET_DIRS": "build/secondary/third_party/mylib",
                    "PLANTER_CHANGE_BASE": "HEAD",
                }
            )
            rc_out = subprocess.run(
                [
                    "bash",
                    rc_script,
                    "--skip-build",
                    "--only",
                    "migration_sanity",
                ],
                env=env,
                check=True,
                capture_output=True,
                text=True,
            ).stdout
            self.assertIn(
                "RESULT: PASS: no blocking findings in live `$PLANTER_PROJECT_DIR` checks (target directory is already migrated at the change base, blocked by unmigrated upstream dependencies, or no longer exists in the checkout, and has no migration diff).",
                rc_out,
            )
            self.assertIn(
                "NEVER add `empty_migration_diff` to `disputed_findings`",
                rc_out,
            )
            self.assertNotIn(
                'empty_migration_diff, "build/secondary/third_party/mylib/BUILD.bazel"',
                rc_out,
            )

        # 4. R4-4: gn_dep_parity #[cfg(test)] exclusion and #[path = "..."] mod expansion.
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/testing/fake_clock/BUILD.gn": (
                        HEADER
                        + 'rustc_library("fake_clock") { sources = [ "src/lib.rs" ] }\n'
                    ),
                    "src/sys/helper/BUILD.gn": (
                        HEADER
                        + 'rustc_library("helper") { sources = [ "src/lib.rs" ] }\n'
                    ),
                    "src/pkg/src/lib.rs": (
                        '#[path = "imp/fuchsia.rs"]\nmod imp;\n'
                        "pub fn run() { imp::go(); }\n"
                        "#[cfg(test)]\nmod tests {\n"
                        "    use fake_clock::advance;\n"
                        "    #[test] fn t() { advance(); }\n"
                        "}\n"
                    ),
                    "src/pkg/src/imp/fuchsia.rs": "pub fn go() { helper::work(); }\n",
                    "src/pkg/BUILD.gn": (
                        HEADER
                        + 'rustc_library("pkg") {\n  sources = [ "src/lib.rs" ]\n}\n'
                    ),
                },
            )
            with open(
                os.path.join(tmpdir, "src/pkg/BUILD.gn"), "w", encoding="utf-8"
            ) as gnf:
                gnf.write(
                    HEADER
                    + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    + 'rustc_library("pkg") {\n'
                    + '  sources = [ "src/lib.rs" ]\n'
                    + '  deps = [ "//src/sys/helper:helper", "//src/testing/fake_clock:fake_clock" ]\n'
                    + "}\n"
                )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PYTHONDONTWRITEBYTECODE": "1",
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/pkg",
                    "PLANTER_TARGET_DIRS": "src/pkg",
                    "PLANTER_CHANGE_BASE": base,
                }
            )
            gdp_out = json.loads(
                subprocess.run(
                    ["bash", gdp_script],
                    env=env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(len(gdp_out), 1)
            self.assertEqual(
                gdp_out[0]["category"], "unexpected_rust_dep_added"
            )
            self.assertIn("fake_clock", gdp_out[0]["message"])
            self.assertNotIn("helper", gdp_out[0]["message"])

    def test_round9_ffx_plugin_impl_spans_and_secondary_minimality(self):
        import sys

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        machinery_dir = os.path.dirname(checks_dir)
        inspect_script = os.path.join(
            machinery_dir, "tools", "inspect_gn_deps.sh"
        )
        tda_script = os.path.join(checks_dir, "test_data_attribute.sh")
        minimality_script = os.path.join(checks_dir, "bazel_minimality.sh")
        if checks_dir not in sys.path:
            sys.path.insert(0, checks_dir)
        import planter_check_lib as pcl

        # 1. R5-1: rust_target_non_test_sources_embed checks Bazel args_srcs / args_crate_root
        # separately from src/lib.rs on ffx_plugin targets.
        with tempfile.TemporaryDirectory() as tmpdir:
            os.makedirs(os.path.join(tmpdir, "pkg/src"), exist_ok=True)
            with open(
                os.path.join(tmpdir, "pkg/src/lib.rs"), "w", encoding="utf-8"
            ) as f:
                f.write("pub fn run() {}\n")
            with open(
                os.path.join(tmpdir, "pkg/src/args.rs"), "w", encoding="utf-8"
            ) as f:
                f.write('pub const S: &str = include_str!("../schema.json");\n')
            self.assertTrue(
                pcl.rust_target_non_test_sources_embed(
                    tmpdir,
                    "pkg",
                    "ffx_demo",
                    'ffx_plugin(name = "ffx_demo", srcs = ["src/lib.rs"], args_srcs = ["src/args.rs"])',
                    "schema.json",
                )
            )
            self.assertTrue(
                pcl.rust_target_non_test_sources_embed(
                    tmpdir,
                    "pkg",
                    "ffx_suite_only",
                    'ffx_plugin(name = "ffx_suite_only", args_srcs = ["src/args.rs"])',
                    "schema.json",
                )
            )

        # 2. R5-2 & R5-3: inspect_gn_deps.sh inspects plugin_deps and resolves ffx_plugin
        # _args, _suite, _config (non-host-only), and _sub_command subtargets.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "src/ffx/sub_ready/BUILD.bazel": (
                        HEADER
                        + "ffx_plugin(\n"
                        + '    name = "ffx_sub",\n'
                        + '    args_srcs = ["src/args.rs"],\n'
                        + '    plugin_deps = ["//src/ffx/leaf:ffx_leaf"],\n'
                        + '    visibility = ["//visibility:private"],\n'
                        + ")\n"
                    ),
                    "src/ffx/parent/BUILD.gn": (
                        HEADER
                        + 'ffx_plugin("ffx_parent") {\n'
                        + "  plugin_deps = [\n"
                        + '    "//src/ffx/sub_unmigrated:ffx_unmig",\n'
                        + '    "//src/ffx/sub_ready:ffx_sub",\n'
                        + "  ]\n"
                        + "  deps = [\n"
                        + '    "//src/ffx/sub_ready:ffx_sub_args",\n'
                        + '    "//src/ffx/sub_ready:ffx_sub_config",\n'
                        + '    "//src/ffx/sub_ready:ffx_sub_sub_command",\n'
                        + "  ]\n"
                        + "}\n"
                    ),
                },
            )
            insp = json.loads(
                subprocess.run(
                    ["bash", inspect_script, "src/ffx/parent"],
                    env={**os.environ, "PLANTER_WORKDIR": tmpdir},
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertIn(
                "//src/ffx/sub_unmigrated",
                insp["missing_external_bazel_packages"],
            )
            self.assertEqual(insp["target_name_mismatches"], [])
            self.assertIn(
                "//src/ffx/sub_ready:ffx_sub_args",
                insp["host_constrained_deps"],
            )
            self.assertIn(
                "//src/ffx/sub_ready:ffx_sub_sub_command",
                insp["host_constrained_deps"],
            )
            self.assertNotIn(
                "//src/ffx/sub_ready:ffx_sub_config",
                insp["host_constrained_deps"],
            )
            vis_targets = {
                (v["gn_label"], v["target"])
                for v in insp["visibility_edits_needed"]
            }
            self.assertIn(
                ("//src/ffx/sub_ready:ffx_sub_args", "ffx_sub"), vis_targets
            )

        # 3. R5-5 & R5-6: balanced Rust impl self-type parsing and unified 2-hop wrapper
        # + qualified generic method matching in test_data_attribute.sh.
        spans = pcl.extract_rust_impl_spans(
            "fn helper() -> impl Iterator<Item = u8> { std::iter::empty() }\n"
            "impl<P: AsRef<Path>> TestEnv<P> { pub fn new() {} }\n"
            "impl<'a> From<&'a str> for inner::FromEnv<'a> { fn from(_s: &'a str) -> Self { todo!() } }\n"
        )
        self.assertEqual([s[2] for s in spans], ["TestEnv", "FromEnv"])

        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/dep/BUILD.bazel": (
                        HEADER
                        + "host_test_data_files(\n"
                        + '    name = "unused_fx",\n'
                        + '    srcs = ["f.json"],\n'
                        + '    dest_dir = "test_data/dep_fx",\n'
                        + ")\n"
                        + "host_test_data_files(\n"
                        + '    name = "generic_fx",\n'
                        + '    srcs = ["g.json"],\n'
                        + '    dest_dir = "test_data/gen_fx",\n'
                        + ")\n"
                        + "rustc_library(\n"
                        + '    name = "dep",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + '    edition = "2024",\n'
                        + ")\n"
                    ),
                    "src/dep/src/lib.rs": (
                        "use std::path::{Path, PathBuf};\n"
                        "pub struct Unused;\n"
                        'impl Unused { pub fn new() -> &\'static str { "test_data/dep_fx/f.json" } }\n'
                        "pub struct TestEnv<P>(P);\n"
                        "impl<P: AsRef<Path>> TestEnv<P> {\n"
                        '    pub fn new(root: P) -> PathBuf { root.as_ref().join("test_data/gen_fx/g.json") }\n'
                        "}\n"
                    ),
                    "src/leaf/BUILD.bazel": (
                        HEADER
                        + "host_test_data_files(\n"
                        + '    name = "leaf_fx",\n'
                        + '    srcs = ["b.bin"],\n'
                        + '    dest_dir = "test_data/leaf_fx",\n'
                        + ")\n"
                        + "rustc_library(\n"
                        + '    name = "leaf",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + '    edition = "2024",\n'
                        + ")\n"
                    ),
                    "src/leaf/src/lib.rs": (
                        'pub fn fixture_dir() -> &\'static str { "test_data/leaf_fx/b.bin" }\n'
                    ),
                    "src/mid/BUILD.bazel": (
                        HEADER
                        + "rustc_library(\n"
                        + '    name = "mid",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + '    edition = "2024",\n'
                        + '    deps = ["//src/leaf:leaf"],\n'
                        + ")\n"
                    ),
                    "src/mid/src/lib.rs": (
                        "pub fn setup_env() -> &'static str { leaf::fixture_dir() }\n"
                    ),
                    "src/caller/src/lib.rs": (
                        "#[test]\n"
                        "fn t() {\n"
                        "    let _v: Vec<u8> = Vec::new();\n"
                        '    let _ = dep::TestEnv::new(".");\n'
                        "    let _ = mid::setup_env();\n"
                        "}\n"
                    ),
                },
            )
            with open(
                os.path.join(tmpdir, "src/caller/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + "host_rustc_test(\n"
                    + '    name = "caller_test",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    deps = ["//src/dep:dep", "//src/mid:mid"],\n'
                    + "    test_data = [\n"
                    + '        "//src/dep:unused_fx",\n'
                    + '        "//src/dep:generic_fx",\n'
                    + '        "//src/leaf:leaf_fx",\n'
                    + "    ],\n"
                    + ")\n"
                )
            tda_out = json.loads(
                subprocess.run(
                    ["bash", tda_script],
                    env={
                        **os.environ,
                        "PLANTER_WORKDIR": tmpdir,
                        "PLANTER_CHANGE_BASE": base,
                        "PLANTER_TARGET_DIR": "src/caller",
                    },
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(len(tda_out), 1)
            self.assertIn("//src/dep:unused_fx", tda_out[0]["message"])

        # 4. R5-4, R5-7, R5-8: bazel_minimality.sh stacked [build] prereq HEAD, Pattern 2.A
        # secondary-tree BUILD.gn elimination, and secondary-tree non_test_deps lookup.
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "build/bazel/rules/h.bzl": HEADER + "X = 1\n",
                    "build/secondary/src/foo/lib/BUILD.gn": (
                        HEADER
                        + 'rustc_library("lib") {\n'
                        + '  sources = [ "src/lib.rs" ]\n'
                        + "  with_unit_tests = true\n"
                        + '  non_test_deps = [ "//src/bar:prod" ]\n'
                        + '  test_deps = [ "//src/bar:fake_bar" ]\n'
                        + "}\n"
                    ),
                    "src/foo/lib/src/lib.rs": "",
                    "src/foo/consumer/BUILD.gn": (
                        HEADER
                        + 'rustc_binary("consumer") { deps = [ "//src/foo/lib" ] }\n'
                    ),
                    "src/foo/consumer/src/main.rs": "fn main() {}\n",
                },
            )
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base)
            with open(
                os.path.join(tmpdir, "build/bazel/rules/h.bzl"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(HEADER + "X = 2\n")
            _git(tmpdir, "add", "build/bazel/rules/h.bzl")
            _git(tmpdir, "commit", "-qm", "[build] Export helper rule")
            os.remove(
                os.path.join(tmpdir, "build/secondary/src/foo/lib/BUILD.gn")
            )
            os.remove(os.path.join(tmpdir, "src/foo/consumer/BUILD.gn"))
            with open(
                os.path.join(tmpdir, "src/foo/lib/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library", "rustc_test")\n\n'
                    + "rustc_library(\n"
                    + '    name = "lib",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    visibility = ["//src/foo:__subpackages__"],\n'
                    + '    deps = ["//src/bar:prod"],\n'
                    + ")\n\n"
                    + "# Unit tests swap //src/bar:prod for //src/bar:fake_bar so crate cannot be used.\n"
                    + "rustc_test(\n"
                    + '    name = "lib_test",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    deps = ["//src/bar:fake_bar"],\n'
                    + ")\n"
                )
            with open(
                os.path.join(tmpdir, "src/foo/consumer/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_binary")\n\n'
                    + "rustc_binary(\n"
                    + '    name = "consumer",\n'
                    + '    srcs = ["src/main.rs"],\n'
                    + '    edition = "2024",\n'
                    + '    deps = ["//src/foo/lib"],\n'
                    + ")\n"
                )
            min_out = json.loads(
                subprocess.run(
                    ["bash", minimality_script],
                    env={
                        k: v
                        for k, v in {
                            **os.environ,
                            "PLANTER_WORKDIR": tmpdir,
                            "PLANTER_CHANGE_BASE": base,
                            "PLANTER_TARGET_DIR": "build/secondary/src/foo/lib",
                        }.items()
                        if k != "PLANTER_RESOLVED_CHANGE_BASE"
                        and k != "PLANTER_CHANGED_FILES"
                    },
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(min_out, [])

    def test_round10_subtargets_remediations_lint_parity_and_truncated_blobs(
        self,
    ):
        import ast
        import re
        import sys
        from unittest import mock

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        machinery_dir = os.path.dirname(checks_dir)
        if checks_dir not in sys.path:
            sys.path.insert(0, checks_dir)
        import planter_check_lib as pcl

        mig_sanity_sh = os.path.join(checks_dir, "migration_sanity.sh")
        bv_sh = os.path.join(checks_dir, "build_verification.sh")
        rc_sh = os.path.join(machinery_dir, "tools", "run_checks.sh")
        lcp_sh = os.path.join(checks_dir, "lint_config_parity.sh")
        gap_sh = os.path.join(checks_dir, "gn_attr_parity.sh")
        cq_sh = os.path.join(checks_dir, "cq_reachability.sh")
        inspect_sh = os.path.join(machinery_dir, "tools", "inspect_gn_deps.sh")

        # 1. R6-1: migration_sanity.sh checks replaced gn_ffx_test_data visibility
        # and does not synthesize fake base subtargets on decomposed ffx_plugin.
        with open(mig_sanity_sh, encoding="utf-8") as mf:
            ms_lines = mf.read().splitlines()
        ms_s = next(
            i
            for i, ln in enumerate(ms_lines)
            if ln.startswith("def _extract_bazel_exports_and_targets(")
        )
        ms_e = next(
            i
            for i, ln in enumerate(ms_lines)
            if i > ms_s
            and ln.startswith("check_preexisting_bazel_exports_and_targets(")
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "src/tool/BUILD.bazel": (
                        HEADER
                        + "gn_ffx_test_data(\n"
                        + '    name = "foo_test_data",\n'
                        + '    visibility = ["//visibility:public"],\n'
                        + ")\n\n"
                        + "gn_ffx_test_data(\n"
                        + '    name = "bar_test_data",\n'
                        + '    visibility = ["//a:__pkg__", "//b:__pkg__"],\n'
                        + ")\n"
                    ),
                    "src/plug/BUILD.bazel": (
                        HEADER + 'ffx_plugin(name = "my_plug")\n'
                    ),
                },
            )
            with open(
                os.path.join(tmpdir, "src/tool/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'ffx_tool(name = "foo")\n'
                    + 'ffx_tool(name = "bar", visibility = ["//a:__pkg__"])\n'
                )
            with open(
                os.path.join(tmpdir, "src/plug/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'rustc_library(name = "my_plug")\n'
                    + 'rustc_library(name = "my_plug_args")\n'
                    + 'json_merge(name = "my_plug_config")\n'
                )
            ms_ns = {
                "ast": ast,
                "json": json,
                "os": os,
                "subprocess": subprocess,
                "workdir": tmpdir,
                "change_base": "HEAD",
                "findings": [],
                "_emit": lambda findings, cat, sev, file, line, msg, rem="": findings.append(
                    {"category": cat, "file": file, "message": msg}
                ),
            }
            exec("\n".join(ms_lines[ms_s:ms_e]), ms_ns)
            ms_ns["check_preexisting_bazel_exports_and_targets"](
                ["src/tool", "src/plug"]
            )
            plug_findings = [
                f
                for f in ms_ns["findings"]
                if f["file"] == "src/plug/BUILD.bazel"
            ]
            tool_findings = [
                f
                for f in ms_ns["findings"]
                if f["file"] == "src/tool/BUILD.bazel"
            ]
            self.assertEqual(plug_findings, [])
            self.assertEqual(len(tool_findings), 2)

        # 2. R6-2, R6-3, R6-10, R6-11, R6-8(cq): consistent remediation strings and
        # list-element stripping in cq_reachability.sh.
        with open(bv_sh, encoding="utf-8") as f:
            bv_txt = f.read()
        self.assertNotIn("fx host-tool shac check", bv_txt)
        self.assertIn("never dispute post-turn", bv_txt)

        with open(rc_sh, encoding="utf-8") as f:
            rc_txt = f.read()
        self.assertNotIn("First commit the migration locally", rc_txt)
        self.assertIn("(otherwise keep Git edits uncommitted)", rc_txt)

        with open(cq_sh, encoding="utf-8") as f:
            cq_txt = f.read()
        self.assertNotIn('empty_migration_diff, "{td_bz}"', cq_txt)
        self.assertIn("-- never disputing ", cq_txt)
        self.assertNotIn("'Depends on:' in the commit message.", cq_txt)
        self.assertIn(
            "`Depends-on: <Change-Id>` in the final footer paragraph.", cq_txt
        )
        cq_py = re.search(
            r"python3[^\n]*<<'PYEOF'\n(.*?)\nPYEOF", cq_txt, flags=re.S
        ).group(1)
        cq_tree = ast.parse(cq_py)
        cq_ns = {"re": re}
        wanted_cq = {
            "gn_code",
            "_gn_args_bracket_delta",
            "_consume_gn_args_stmt",
            "_find_top_gn_args_bracket_spans",
            "_strip_pm_args_comment_from_tail",
            "_filter_planter_managed_args_stmt",
            "_strip_planter_managed_args_gn",
        }
        wanted_cq_vars = {
            "_PLANTER_MANAGED_ARGS_RE",
            "_GN_ARGS_LABEL_ASSIGN_RE",
        }
        cq_sub = [
            n
            for n in cq_tree.body
            if (isinstance(n, ast.FunctionDef) and n.name in wanted_cq)
            or (
                isinstance(n, ast.Assign)
                and any(
                    isinstance(t, ast.Name) and t.id in wanted_cq_vars
                    for t in n.targets
                )
            )
        ]
        exec(
            compile(ast.Module(body=cq_sub, type_ignores=[]), cq_sh, "exec"),
            cq_ns,
        )
        stripped_cq = cq_ns["_strip_planter_managed_args_gn"](
            "host_labels += [\n"
            '  "//bundles/buildbot/host",\n'
            "  # planter-managed\n"
            '  "//src/foo:tests",\n'
            "]\n"
        )
        self.assertIn('"//bundles/buildbot/host"', stripped_cq)
        self.assertNotIn("//src/foo:tests", stripped_cq)

        # 3. R6-4: lint_config_parity.sh delegates change_base / changed_files and
        # supports build/secondary/<dir> base BUILD.gn lookups.
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/area/sub/BUILD.gn": (
                        HEADER + 'rustc_library("sub") { edition = "2024" }\n'
                    ),
                    "src/area/sub/lib.rs": "use std::fmt;\npub fn f() {}\n",
                    "build/secondary/third_party/rlib/BUILD.gn": (
                        HEADER + 'rustc_library("rlib") { edition = "2024" }\n'
                    ),
                    "third_party/rlib/src/lib.rs": "pub fn g() {}\n",
                },
            )
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base)
            with open(
                os.path.join(tmpdir, "src/area/sub/lib.rs"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write("pub fn f() {}\n")
            os.makedirs(os.path.join(tmpdir, "src/area/sub"), exist_ok=True)
            with open(
                os.path.join(tmpdir, "src/area/sub/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + 'rustc_library(name = "sub", srcs = ["lib.rs"], edition = "2024")\n'
                )
            _git(
                tmpdir, "add", "src/area/sub/lib.rs", "src/area/sub/BUILD.bazel"
            )
            _git(
                tmpdir,
                "commit",
                "-qm",
                "[bazel_migration] //src/area/sub\n\nBazel-Migration-Target: //src/area/sub:sub",
            )
            os.makedirs(os.path.join(tmpdir, "src/area"), exist_ok=True)
            with open(
                os.path.join(tmpdir, "src/area/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(HEADER + 'test_suite(name = "tests", tests = [])\n')
            lcp_env = {
                k: v
                for k, v in {
                    **os.environ,
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/area",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_CHANGED_FILES": "src/area/sub/lib.rs\nsrc/area/BUILD.bazel",
                }.items()
                if k != "PLANTER_RESOLVED_CHANGE_BASE"
            }
            lcp_out = json.loads(
                subprocess.run(
                    ["bash", lcp_sh],
                    env=lcp_env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(lcp_out, [])

            with open(
                os.path.join(tmpdir, "third_party/rlib/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + 'rustc_library(name = "rlib", srcs = ["src/lib.rs"], edition = "2021")\n'
                )
            lcp_sec = json.loads(
                subprocess.run(
                    ["bash", lcp_sh],
                    env={
                        **lcp_env,
                        "PLANTER_TARGET_DIR": "build/secondary/third_party/rlib",
                        "PLANTER_CHANGED_FILES": "third_party/rlib/BUILD.bazel",
                    },
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(
                [f["category"] for f in lcp_sec], ["rust_edition_downgrade"]
            )

        # 4. R6-5: gn_attr_parity.sh checks retained GN disable_rustdoc below sentinel.
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/rlib/BUILD.gn": (
                        HEADER
                        + 'rustc_library("rlib") {\n'
                        + '  sources = [ "src/lib.rs" ]\n'
                        + "  disable_rustdoc = true\n"
                        + "}\n"
                    ),
                    "src/rlib/src/lib.rs": "pub fn f() {}\n",
                },
            )
            with open(
                os.path.join(tmpdir, "src/rlib/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + 'rustc_library(name = "rlib", srcs = ["src/lib.rs"], edition = "2024")\n'
                )
            with open(
                os.path.join(tmpdir, "src/rlib/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    + 'rustc_library("rlib") {\n'
                    + '  sources = [ "src/lib.rs" ]\n'
                    + "}\n"
                )
            gap_env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            gap_env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/rlib",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_RESOLVED_CHANGE_BASE": base,
                    "PLANTER_SKIP_DIFF_COVERAGE": "1",
                }
            )
            gap_drop = json.loads(
                subprocess.run(
                    ["bash", gap_sh],
                    env=gap_env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(
                [
                    f["category"]
                    for f in gap_drop
                    if f["category"] == "gn_only_attr_dropped"
                ],
                ["gn_only_attr_dropped"],
            )

        # 5. R6-7: find_truncated_head_build_blobs recovers under PLANTER_CHANGE_BASE=HEAD
        # and ignores intentional Case 1 build/secondary/<dir>/BUILD.gn deletion.
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/foo/BUILD.gn": HEADER + 'rustc_library("foo") {}\n',
                    "build/secondary/third_party/bar/BUILD.gn": (
                        HEADER + 'source_set("bar") {}\n'
                    ),
                },
            )
            _git(tmpdir, "update-ref", "refs/remotes/origin/main", base)
            with open(
                os.path.join(tmpdir, "src/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER + 'rustc_library(name = "foo", srcs = ["lib.rs"])\n'
                )
            _git(tmpdir, "add", "src/foo/BUILD.bazel")
            _git(
                tmpdir,
                "commit",
                "-qm",
                "[bazel_migration] //src/foo\n\nBazel-Migration-Target: //src/foo:foo\nTAG: planter",
            )
            with open(
                os.path.join(tmpdir, "src/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write("")
            clean_env = {
                k: v
                for k, v in os.environ.items()
                if k
                not in (
                    "PLANTER_RESOLVED_CHANGE_BASE",
                    "PLANTER_CHANGED_FILES",
                    "PLANTER_TARGET_DIRS",
                )
            }
            clean_env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/foo",
                    "PLANTER_CHANGE_BASE": "HEAD",
                }
            )
            with mock.patch.dict(os.environ, clean_env, clear=True):
                rec_head = {
                    os.path.relpath(k, tmpdir)
                    for k in pcl.find_truncated_head_build_blobs(tmpdir)
                }
                self.assertIn("src/foo/BUILD.bazel", rec_head)

            with open(
                os.path.join(tmpdir, "src/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER + 'rustc_library(name = "foo", srcs = ["lib.rs"])\n'
                )
            os.makedirs(os.path.join(tmpdir, "third_party/bar"), exist_ok=True)
            with open(
                os.path.join(tmpdir, "third_party/bar/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(HEADER + 'cc_library(name = "bar")\n')
            with open(
                os.path.join(
                    tmpdir, "build/secondary/third_party/bar/BUILD.gn"
                ),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(HEADER + '## BAZEL2GN SENTINEL\nsource_set("bar") {}\n')
            _git(
                tmpdir,
                "add",
                "src/foo/BUILD.bazel",
                "third_party/bar/BUILD.bazel",
                "build/secondary/third_party/bar/BUILD.gn",
            )
            _git(
                tmpdir,
                "commit",
                "-qm",
                "[bazel_migration] //third_party/bar\n\nBazel-Migration-Target: //third_party/bar:bar\nTAG: planter",
            )
            os.unlink(
                os.path.join(tmpdir, "build/secondary/third_party/bar/BUILD.gn")
            )
            clean_env.update(
                {
                    "PLANTER_TARGET_DIR": "build/secondary/third_party/bar",
                    "PLANTER_CHANGE_BASE": base,
                }
            )
            with mock.patch.dict(os.environ, clean_env, clear=True):
                rec_sec = {
                    os.path.relpath(k, tmpdir)
                    for k in pcl.find_truncated_head_build_blobs(tmpdir)
                }
                self.assertEqual(rec_sec, set())

        # 6. R6-9: inspect_gn_deps.sh unwraps aliased load() keyword constant values.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "build/vis/defs.bzl": (
                        HEADER
                        + 'ALLOWLIST = ["//src/caller:__subpackages__"]\n'
                    ),
                    "src/dep/BUILD.bazel": (
                        HEADER
                        + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n'
                        + 'load("//build/vis:defs.bzl", _VIS = "ALLOWLIST")\n\n'
                        + "rustc_library(\n"
                        + '    name = "dep",\n'
                        + '    srcs = ["lib.rs"],\n'
                        + "    visibility = _VIS,\n"
                        + ")\n"
                    ),
                    "src/caller/BUILD.gn": (
                        HEADER
                        + 'rustc_library("caller") { deps = [ "//src/dep:dep" ] }\n'
                    ),
                },
            )
            insp_alias = json.loads(
                subprocess.run(
                    ["bash", inspect_sh, "src/caller"],
                    env={**os.environ, "PLANTER_WORKDIR": tmpdir},
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(insp_alias["visibility_edits_needed"], [])
            self.assertEqual(len(insp_alias["ready_deps"]), 1)

    def test_round11_compound_args_secondary_overlays_and_bzl_symbols(self):
        """Regression tests for Round 7 findings across machinery checks and tools."""
        import ast
        import re
        import time

        checks_dir = os.path.dirname(SCRIPT)

        def _extract_heredoc_python(sh_path):
            with open(sh_path, encoding="utf-8") as sf:
                m_hd = re.search(
                    r"python3[^\n]*<<'PYEOF'\n(.*?)\nPYEOF",
                    sf.read(),
                    flags=re.S,
                )
            self.assertIsNotNone(m_hd)
            return m_hd.group(1)

        inspect_sh = os.path.normpath(
            os.path.join(checks_dir, "..", "tools", "inspect_gn_deps.sh")
        )
        bv_py = _extract_heredoc_python(
            os.path.join(checks_dir, "build_verification.sh")
        )
        cq_py = _extract_heredoc_python(
            os.path.join(checks_dir, "cq_reachability.sh")
        )

        def _extract_fn_defs(src, fn_names):
            tree = ast.parse(src)
            picked = []
            want = set(fn_names)
            for node in tree.body:
                if isinstance(node, ast.FunctionDef) and node.name in want:
                    picked.append(ast.get_source_segment(src, node))
            return "\n\n".join(picked)

        # 1. R7-4: cq_reachability.sh preserves compound args.gn expressions.
        cq_ns = {
            "re": re,
            "_PLANTER_MANAGED_ARGS_RE": re.compile(
                r"#\s*planter-managed\b", re.I
            ),
            "_GN_ARGS_LABEL_ASSIGN_RE": re.compile(
                r"^\s*([A-Za-z_]\w*_labels)\s*(\+?=|-=)"
            ),
        }
        exec(
            _extract_fn_defs(
                cq_py,
                [
                    "gn_code",
                    "_gn_args_bracket_delta",
                    "_consume_gn_args_stmt",
                    "_find_top_gn_args_bracket_spans",
                    "_strip_pm_args_comment_from_tail",
                    "_filter_planter_managed_args_stmt",
                    "_strip_planter_managed_args_gn",
                ],
            ),
            cq_ns,
        )
        self.assertEqual(
            cq_ns["_strip_planter_managed_args_gn"](
                "host_labels = base_labels + [\n"
                '  "//src/foo:tests",  # planter-managed\n'
                "]\n"
                'developer_test_labels = dev_base + [ "//src/bar:tests" ]  # planter-managed\n'
                "target_labels += extra_base + [\n"
                '  "//src/baz:baz",  # planter-managed\n'
                "]\n"
            ),
            "host_labels = base_labels + []\n"
            "developer_test_labels = dev_base + []\n"
            "target_labels += extra_base + []\n",
        )

        # 2. R7-1 & R7-6: build_verification.sh preserves user/compound args.gn labels,
        # resolves build/secondary/ labels, and classifies secondary preexisting bitrot.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "build/secondary/third_party/sec_ok/BUILD.gn": (
                        HEADER + 'source_set("sec_ok") {}\n'
                    ),
                    "build/secondary/third_party/broken/BUILD.gn": (
                        HEADER
                        + 'source_set("broken") { sources = [ "bad.cc" ] }\n'
                    ),
                    "third_party/broken/bad.cc": "broken c++\n",
                },
            )
            base_sha = _git(tmpdir, "rev-parse", "HEAD")
            bv_ns = {
                "os": os,
                "re": re,
                "json": json,
                "subprocess": subprocess,
                "tempfile": tempfile,
                "time": time,
                "workdir": tmpdir,
                "target_dirs": [
                    "build/secondary/third_party/broken",
                    "build/secondary/third_party/sec_ok",
                ],
                "changed_set": set(),
                "has_local_commit": False,
                "worktree_dirty_vs_head": False,
                "head_sha": base_sha,
                "base_sha": base_sha,
                "LOCATION": re.compile(
                    r"(?:^|\s)(\.\./\.\./[^\s:(]+|[A-Za-z0-9_./+-]+\.(?:cc|rs|h)):\d+",
                    re.M,
                ),
                "_PLANTER_MANAGED_COMMENT_RE": re.compile(
                    r"#\s*planter-managed\b", re.I
                ),
                "_ARGS_GN_LABEL_ASSIGN_RE": re.compile(
                    r"^\s*([A-Za-z_]\w*_labels)\s*(\+?=|-=)"
                ),
                "gn_code": cq_ns["gn_code"],
                "read": lambda rel: (
                    open(os.path.join(tmpdir, rel), encoding="utf-8").read()
                    if os.path.isfile(os.path.join(tmpdir, rel))
                    else None
                ),
                "label_dir": lambda s, cur: (
                    s[2:].partition(":")[0].strip("/")
                    if s.startswith("//")
                    else cur
                ),
                "_existed_at_change_base": lambda rel: os.path.isfile(
                    os.path.join(tmpdir, rel)
                ),
                "_path_in_pkg_dirs": lambda p, dirs: any(
                    p == d or p.startswith(d + "/") for d in dirs
                ),
            }
            exec(
                _extract_fn_defs(
                    bv_py,
                    [
                        "_owning_pkg_dir",
                        "_record_preexisting_gn_failure_pkgs",
                        "_unmodified_failing_source_pkgs",
                        "_pkg_dir_variants",
                        "_has_pkg_bazel_build",
                        "_expand_pkgs_for_preexisting_record",
                        "preexisting_unmodified_gn_failure",
                        "_blank_gn_comments_bv",
                        "_gn_closing_brace_bv",
                        "_args_gn_bracket_delta",
                        "_consume_args_gn_stmt",
                        "_pos_inside_is_host_gn_block",
                        "_planter_args_gn_label_resolves",
                        "_find_top_args_gn_bracket_spans_bv",
                        "_strip_pm_comment_from_tail_bv",
                        "_rewrite_planter_managed_stmt",
                        "_strip_stale_planter_managed_args_gn",
                    ],
                ),
                bv_ns,
            )
            mixed_bv_args = (
                "target_labels = base_labels + [\n"
                '  "//tools/user_macro_generated",\n'
                '  "//third_party/sec_ok:sec_ok",  # planter-managed\n'
                '  "//src/gone:gone",  # planter-managed\n'
                "]\n"
            )
            rewritten_bv = bv_ns["_strip_stale_planter_managed_args_gn"](
                mixed_bv_args
            )
            self.assertIn("base_labels + [", rewritten_bv)
            self.assertIn('"//tools/user_macro_generated"', rewritten_bv)
            self.assertIn('"//third_party/sec_ok:sec_ok"', rewritten_bv)
            self.assertNotIn("//src/gone:gone", rewritten_bv)

            note_unmod = bv_ns["preexisting_unmodified_gn_failure"](
                "gn_build",
                1,
                "../../third_party/broken/bad.cc:1:1: error: unknown type\n",
            )
            self.assertIsNotNone(note_unmod)
            rec_path = os.path.join(
                tmpdir, ".git", "planter-preexisting-gn-failures.json"
            )
            with open(rec_path, encoding="utf-8") as rf:
                rec_entries = json.load(rf)["entries"]
            self.assertIn(
                f"{base_sha}:build/secondary/third_party/broken", rec_entries
            )
            self.assertNotIn(
                f"{base_sha}:build/secondary/third_party/sec_ok", rec_entries
            )

        # 3. R7-2 & R7-3: migration_sanity.sh and run_checks.sh handle secondary overlays.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "build/secondary/third_party/rust_sec/BUILD.gn": (
                        HEADER
                        + 'rustc_library("rust_sec") {\n'
                        + '  sources = [ "src/lib.rs" ]\n'
                        + '  public_configs = [ "//build/config:dynamic_cpp_standard_library" ]\n'
                        + "}\n"
                    ),
                    "third_party/rust_sec/src/lib.rs": "pub fn f() {}\n",
                },
            )
            base_sha = _git(tmpdir, "rev-parse", "HEAD")
            os.unlink(
                os.path.join(
                    tmpdir, "build/secondary/third_party/rust_sec/BUILD.gn"
                )
            )
            with open(
                os.path.join(tmpdir, "third_party/rust_sec/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library")\n\n'
                    + "rustc_library(\n"
                    + '    name = "rust_sec",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    link_deps = ["//src/cpp:ffi"],\n'
                    + ")\n"
                )
            ms_env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            ms_env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "build/secondary/third_party/rust_sec",
                    "PLANTER_TARGET_DIRS": "build/secondary/third_party/rust_sec",
                    "PLANTER_CHANGE_BASE": base_sha,
                    "PLANTER_RESOLVED_CHANGE_BASE": base_sha,
                    "PLANTER_CHANGED_FILES": (
                        "build/secondary/third_party/rust_sec/BUILD.gn\n"
                        "third_party/rust_sec/BUILD.bazel"
                    ),
                }
            )
            ms_out = json.loads(
                subprocess.run(
                    ["bash", os.path.join(checks_dir, "migration_sanity.sh")],
                    env=ms_env,
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(
                [
                    f
                    for f in ms_out
                    if f["category"] == "target_directory_missing"
                ],
                [],
            )

        # 4. R7-5: bazel_minimality.sh scopes workspace .bzl inspection to loaded symbol.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "src/barrel/clean_first.bzl": (
                        HEADER + "def clean_macro(name, **kwargs):\n    pass\n"
                    ),
                    "src/barrel/legacy_second.bzl": (
                        HEADER
                        + 'load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")\n'
                        + "def legacy_macro(name, **kwargs):\n"
                        + "    native.cc_library(name = name, target_compatible_with = HOST_CONSTRAINTS, **kwargs)\n"
                    ),
                    "src/barrel/defs.bzl": (
                        HEADER
                        + 'load("//src/barrel:clean_first.bzl", "clean_macro")\n'
                        + 'load("//src/barrel:legacy_second.bzl", "legacy_macro")\n'
                    ),
                    "src/barrel/mixed.bzl": (
                        HEADER
                        + 'load("//src/barrel:legacy_second.bzl", "legacy_macro")\n'
                        + "def safe_def(name, **kwargs):\n"
                        + "    native.cc_library(name = name, **kwargs)\n"
                    ),
                    "bundles/infra/test/BUILD.gn": (
                        HEADER
                        + 'group("test") { deps = [ "//tools/t1:t1", "//tools/t2:t2" ] }\n'
                    ),
                    "src/dep_bad/BUILD.bazel": (
                        HEADER
                        + 'load("//src/barrel:defs.bzl", "legacy_macro")\n\n'
                        + 'legacy_macro(name = "dep_bad")\n'
                    ),
                    "src/dep_ok/BUILD.bazel": (
                        HEADER
                        + 'load("//src/barrel:mixed.bzl", "safe_def")\n\n'
                        + 'safe_def(name = "dep_ok")\n'
                    ),
                    "tools/t1/BUILD.gn": HEADER + 'cc_binary("t1") {}\n',
                    "tools/t2/BUILD.gn": HEADER + 'cc_binary("t2") {}\n',
                },
            )
            base_sha = _git(tmpdir, "rev-parse", "HEAD")
            os.unlink(os.path.join(tmpdir, "tools/t1/BUILD.gn"))
            os.unlink(os.path.join(tmpdir, "tools/t2/BUILD.gn"))
            with open(
                os.path.join(tmpdir, "tools/t1/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/bazel_idk:defs.bzl", "cc_binary_host_tool")\n\n'
                    + 'cc_binary_host_tool(name = "t1", deps = ["//src/dep_bad:dep_bad"])\n'
                )
            with open(
                os.path.join(tmpdir, "tools/t2/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/bazel_idk:defs.bzl", "cc_binary_host_tool")\n\n'
                    + 'cc_binary_host_tool(name = "t2", deps = ["//src/dep_ok:dep_ok"])\n'
                )
            bm_out = _run_minimality(
                tmpdir,
                base_sha,
                "tools/t1,tools/t2",
                [
                    "tools/t1/BUILD.gn",
                    "tools/t1/BUILD.bazel",
                    "tools/t2/BUILD.gn",
                    "tools/t2/BUILD.bazel",
                ],
            )
            hc_files = [
                f["file"]
                for f in bm_out
                if f["category"] == "transitive_legacy_host_constraints"
            ]
            self.assertIn("tools/t1/BUILD.bazel", hc_files)
            self.assertNotIn("tools/t2/BUILD.bazel", hc_files)

        # 5. R7-7: inspect_gn_deps.sh supports BUILD files and overlaid build/secondary/ packages.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "third_party/skylib/BUILD": (
                        HEADER
                        + 'exports_files(["data.txt"], visibility = ["//visibility:public"])\n'
                        + 'cc_library(name = "skylib", visibility = ["//visibility:public"])\n'
                    ),
                    "third_party/skylib/data.txt": "ok\n",
                    "build/secondary/third_party/sec_caller/BUILD.gn": (
                        HEADER
                        + 'source_set("sec_caller") {\n'
                        + '  deps = [ "//third_party/skylib:skylib" ]\n'
                        + '  inputs = [ "//third_party/skylib/data.txt" ]\n'
                        + "}\n"
                    ),
                },
            )
            insp_sec = json.loads(
                subprocess.run(
                    ["bash", inspect_sh, "third_party/sec_caller"],
                    env={**os.environ, "PLANTER_WORKDIR": tmpdir},
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertTrue(insp_sec["has_build_gn"])
            self.assertEqual(insp_sec["missing_external_bazel_packages"], [])
            self.assertEqual(insp_sec["inputs_status"][0]["status"], "ready")

    def test_round12_round8_review_fixes(self):
        import ast
        import re

        import planter_check_lib as pcl

        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.join(os.path.dirname(checks_dir), "tools")
        inspect_sh = os.path.join(tools_dir, "inspect_gn_deps.sh")

        def _py_heredoc(sh_path):
            with open(sh_path, encoding="utf-8") as sf:
                txt = sf.read()
            m = re.search(r"<<'PYEOF'\n(.*?)\nPYEOF", txt, re.S)
            assert m is not None, sh_path
            return m.group(1)

        def _extract_defs(src, names):
            tree = ast.parse(src)
            want = set(names)
            picked = []
            for node in tree.body:
                if isinstance(node, ast.FunctionDef) and node.name in want:
                    picked.append(ast.get_source_segment(src, node))
                elif isinstance(node, ast.Assign) and any(
                    isinstance(t, ast.Name) and t.id in want
                    for t in node.targets
                ):
                    picked.append(ast.get_source_segment(src, node))
            return "\n\n".join(picked)

        ms_py = _py_heredoc(os.path.join(checks_dir, "migration_sanity.sh"))
        tjp_py = _py_heredoc(os.path.join(checks_dir, "tests_json_parity.sh"))

        # 1. R8-1: _inspect_system_stub_shared_lib does not fall through to unrelated siblings.
        with tempfile.TemporaryDirectory() as tmpdir:
            os.makedirs(os.path.join(tmpdir, "sdk/lib/foo"), exist_ok=True)
            with open(
                os.path.join(tmpdir, "sdk/lib/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + "cc_link_stub_from_ifs_file(\n"
                    + '    name = "foo_stub",\n'
                    + '    ifs_file = "libfoo.ifs",\n'
                    + "    system_provided = True,\n"
                    + ")\n"
                    + 'idk_cc_shared_library(name = "foo")\n'
                )
            with open(
                os.path.join(tmpdir, "sdk/lib/foo/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'sdk_shared_library("other_unmigrated") {}\n'
                    + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    + 'sdk_shared_library("foo") {}\n'
                )
            ns_ifs = {
                "ast": ast,
                "os": os,
                "re": re,
                "workdir": tmpdir,
                "parse_gn_file": lambda _p: {
                    "pre_targets": {
                        "other_unmigrated": ("sdk_shared_library", 1, "")
                    },
                    "post_targets": {"foo": ("sdk_shared_library", 3, "")},
                },
            }
            exec(
                _extract_defs(
                    ms_py,
                    [
                        "_bazel_pkg_targets_cache",
                        "_get_pkg_bazel_targets_map",
                        "_collect_named_top_level_calls",
                        "_system_stub_shlib_cache",
                        "_REAL_BAZEL_SHLIB_RULES",
                        "_inspect_system_stub_shared_lib",
                    ],
                ),
                ns_ifs,
            )
            self.assertIsNone(
                ns_ifs["_inspect_system_stub_shared_lib"](
                    "sdk/lib/foo", "foo_stub"
                )
            )

        # 3. R8-4: tests_json_parity.sh _gn_root_pkg_has_build_gn checks build/secondary/.
        with tempfile.TemporaryDirectory() as tmpdir:
            os.makedirs(
                os.path.join(tmpdir, "build/secondary/third_party/foo"),
                exist_ok=True,
            )
            with open(
                os.path.join(
                    tmpdir, "build/secondary/third_party/foo/BUILD.gn"
                ),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(HEADER + 'group("tests") { testonly = true }\n')
            import verify_tests_json_parity as vtp

            tjp_ns = {
                "os": os,
                "re": re,
                "vtp": vtp,
                "workdir": tmpdir,
                "touched_dirs": {"third_party/foo"},
            }
            exec(_extract_defs(tjp_py, ["_gn_root_pkg_has_build_gn"]), tjp_ns)
            self.assertTrue(
                tjp_ns["_gn_root_pkg_has_build_gn"]("//third_party/foo:tests")
            )

        # 4. R8-5: extract_gn_decomposed_attr_items rejects forward_variables_from and unknown templates.
        self.assertEqual(
            pcl.extract_gn_decomposed_attr_items(
                '_common = {\n  test_deps = [ "//src/lib/fuchsia" ]\n}\n'
                'rustc_library("foo") {\n  forward_variables_from(_common, "*")\n}\n',
                "foo",
                "test_deps",
            ),
            (False, []),
        )
        self.assertEqual(
            pcl.extract_gn_decomposed_attr_items(
                'unknown_rust_macro("foo") {\n  test_deps = [ "//src/lib/a" ]\n}\n',
                "foo",
                "test_deps",
            ),
            (False, []),
        )
        self.assertEqual(
            pcl.extract_gn_decomposed_attr_items(
                'template("wrap_foo") {\n'
                '  imported_helper(target_name) {\n    forward_variables_from(invoker, "*")\n  }\n'
                "}\n"
                'wrap_foo("foo") {\n  test_deps = [ "//src/lib/a" ]\n}\n',
                "foo",
                "test_deps",
            ),
            (False, []),
        )
        self.assertEqual(
            pcl.extract_gn_decomposed_attr_items(
                'template("wrap_ok") {\n'
                '  rustc_library(target_name) {\n    forward_variables_from(invoker, "*")\n  }\n'
                "}\n"
                'wrap_ok("foo") {\n  test_deps = [ "//src/lib/a" ]\n}\n',
                "foo",
                "test_deps",
            ),
            (True, ["//src/lib/a"]),
        )

        # 5. R8-6 & R8-7: Secondary BUILD.gn in bazel_minimality.sh and sentinel_drift waivers.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "third_party/foo/src/lib.rs": (
                        "#[cfg(test)]\nmod tests { #[test] fn t() {} }\n"
                    ),
                    "build/secondary/third_party/foo/BUILD.gn": (
                        HEADER
                        + 'rustc_library("foo") {\n'
                        + '  name = "foo_lib"\n'
                        + "  with_unit_tests = true\n"
                        + '  sources = [ "src/lib.rs" ]\n'
                        + "}\n"
                    ),
                    "build/tools/bazel2gn/constants.go": (
                        '"test_suite": noGNEquivalent,\n'
                    ),
                },
            )
            base_sha = _git(tmpdir, "rev-parse", "HEAD")
            with open(
                os.path.join(
                    tmpdir, "build/secondary/third_party/foo/BUILD.gn"
                ),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                )
            with open(
                os.path.join(tmpdir, "third_party/foo/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_library", "rustc_test")\n\n'
                    + 'package(default_visibility = ["//visibility:public"])\n\n'
                    + "rustc_library(\n"
                    + '    name = "foo",\n'
                    + '    crate_name = "foo_lib",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + ")\n\n"
                    + "rustc_test(\n"
                    + '    name = "foo_test",\n'
                    + '    crate = ":foo",\n'
                    + "    # Avoid lib_test collision on GN output_name via bazel2gn.\n"
                    + '    crate_name = "foo_lib_lib_test",\n'
                    + ")\n\n"
                    + 'test_suite(name = "tests", tests = [":foo_test"])\n'
                )
            bm_sec = _run_minimality(
                tmpdir,
                base_sha,
                "build/secondary/third_party/foo",
                [
                    "build/secondary/third_party/foo/BUILD.gn",
                    "third_party/foo/BUILD.bazel",
                ],
            )
            self.assertEqual(bm_sec, [])

        # 6. R8-8: inspect_gn_deps.sh normalizes package-relative GN inputs.
        with tempfile.TemporaryDirectory() as tmpdir:
            _init_repo(
                tmpdir,
                {
                    "src/pkg/BUILD.gn": (
                        HEADER
                        + 'rustc_test("pkg_test") {\n'
                        + "  inputs = [\n"
                        + '    "../shared/fixture.json",\n'
                        + '    "subpkg/golden.bin",\n'
                        + "  ]\n"
                        + "}\n"
                    ),
                    "src/shared/BUILD.bazel": (
                        HEADER
                        + 'package(default_visibility = ["//visibility:public"])\n'
                    ),
                    "src/pkg/subpkg/BUILD.bazel": (
                        HEADER
                        + 'package(default_visibility = ["//visibility:public"])\n'
                    ),
                    "src/shared/fixture.json": "{}\n",
                    "src/pkg/subpkg/golden.bin": "{}\n",
                },
            )
            insp_rel = json.loads(
                subprocess.run(
                    ["bash", inspect_sh, "src/pkg"],
                    env={**os.environ, "PLANTER_WORKDIR": tmpdir},
                    check=True,
                    capture_output=True,
                    text=True,
                ).stdout
            )
            self.assertEqual(
                sorted(i["gn_input"] for i in insp_rel["inputs_status"]),
                ["//src/pkg/subpkg/golden.bin", "//src/shared/fixture.json"],
            )
            self.assertTrue(
                all(
                    i["status"] == "export_needed"
                    for i in insp_rel["inputs_status"]
                )
            )


class BazelMinimalityScopeAndMacroTest(unittest.TestCase):
    def test_restricted_directories_and_allowlists(self):
        gen_rel = "build/bazel/rules/rust/generate_unit_tests.bzl"
        gen_footer = (
            "\n\ndef is_allowed(pkg):\n    return pkg in _ALLOWED_PACKAGES\n"
        )
        gen_base = (
            HEADER
            + '"""Unit test allowlist."""\n\n'
            + "_ALLOWED_PACKAGES = {\n"
            + '    "src/keep": True,\n'
            + '    "src/remove": True,\n'
            + "}"
            + gen_footer
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "README.md": "init\n",
                    gen_rel: gen_base,
                    "sdk/history/NEXT/foo.api_summary.json": "{}\n",
                    "zircon/vdso/zx.fidl": "library zx;\n",
                    "build/config/foo/BUILD.gn": 'config("foo") {}\n',
                    "build/bazel/rules/foo.bzl": HEADER
                    + '"""Rules."""\n\nX = []\n',
                    "build/foo/BUILD.gn": 'group("foo") {}\n',
                    "build/beads/planter/doc.md": "# Doc\n",
                    "sdk/atom_lists.bzl": HEADER
                    + '"""Atoms."""\n\n'
                    + "A = [\n"
                    + '    "//src/keep:keep_idk",\n'
                    + "]\n",
                    "sdk/fidl/category_lists.bzl": HEADER
                    + '"""FIDL."""\n\n'
                    + "F = [\n"
                    + '    "//src/keep:keep_idk",\n'
                    + "]\n",
                },
            )
            edits = {
                "sdk/history/NEXT/foo.api_summary.json": '{"v": 1}\n',
                "zircon/vdso/zx.fidl": "library zx;\n// edit\n",
                "build/config/foo/BUILD.gn": 'config("foo") { defines = ["A"] }\n',
                "build/bazel/rules/foo.bzl": HEADER
                + '"""Rules."""\n\nX = ["a"]\n',
                "build/foo/BUILD.gn": 'group("foo") { testonly = true }\n',
                "build/beads/planter/doc.md": "# Updated\n",
                "sdk/atom_lists.bzl": HEADER
                + '"""Atoms."""\n\n'
                + "A = [\n"
                + '    "//src/foo:foo_idk",\n'
                + '    "//src/keep:keep_idk",\n'
                + "]\n",
                "sdk/fidl/category_lists.bzl": HEADER
                + '"""FIDL."""\n\n'
                + "F = [\n"
                + '    "//src/foo:foo_idk",\n'
                + '    "//src/keep:keep_idk",\n'
                + "]\n",
                gen_rel: (
                    HEADER
                    + '"""Unit test allowlist."""\n\n'
                    + "_ALLOWED_PACKAGES = {\n"
                    + '    "src/keep": True,\n'
                    + "}"
                    + gen_footer
                ),
            }
            for rel, content in edits.items():
                with open(
                    os.path.join(tmpdir, rel), "w", encoding="utf-8"
                ) as f:
                    f.write(content)
            _git(tmpdir, "add", ".")
            _git(tmpdir, "commit", "-q", "-m", "[bazel_migration] edits")

            findings = _run_minimality(
                tmpdir, base, "src/foo", list(edits.keys())
            )
            by_file = {f["file"]: f["category"] for f in findings}
            self.assertEqual(
                by_file,
                {
                    "sdk/history/NEXT/foo.api_summary.json": "out_of_scope_package_modified",
                    "zircon/vdso/zx.fidl": "zircon_vdso_modification_forbidden",
                    "build/config/foo/BUILD.gn": "build_config_modification_forbidden",
                    "build/bazel/rules/foo.bzl": "bazel_rules_modification_forbidden",
                    "build/foo/BUILD.gn": "build_directory_modification_forbidden",
                },
            )

    def test_generate_unit_tests_additions_forbidden(self):
        gen_rel = "build/bazel/rules/rust/generate_unit_tests.bzl"
        gen_footer = (
            "\n\ndef is_allowed(pkg):\n    return pkg in _ALLOWED_PACKAGES\n"
        )
        gen_base = (
            HEADER
            + '"""Unit test allowlist."""\n\n'
            + "_ALLOWED_PACKAGES = {\n"
            + '    "src/keep": True,\n'
            + "}"
            + gen_footer
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {gen_rel: gen_base})
            with open(
                os.path.join(tmpdir, gen_rel), "w", encoding="utf-8"
            ) as f:
                f.write(
                    HEADER
                    + '"""Unit test allowlist."""\n\n'
                    + "_ALLOWED_PACKAGES = {\n"
                    + '    "src/add": True,\n'
                    + '    "src/keep": True,\n'
                    + "}"
                    + gen_footer
                )
            _git(tmpdir, "add", ".")
            _git(
                tmpdir,
                "commit",
                "-q",
                "-m",
                "[bazel_migration] add allowlist entry",
            )

            findings = _run_minimality(tmpdir, base, "src/add", [gen_rel])
            self.assertEqual(
                [(f["file"], f["category"]) for f in findings],
                [(gen_rel, "bazel_rules_modification_forbidden")],
            )

    def test_macro_and_rule_modifications_forbidden(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/foo/defs.bzl": HEADER + '"""Macros."""\n',
                    "src/foo/defs.gni": 'import("//build/config/clang/clang.gni")\n',
                },
            )
            edits = {
                "src/foo/defs.bzl": HEADER
                + '"""Macros."""\n\n'
                + "def my_macro(name):\n"
                + "    native.filegroup(name = name)\n",
                "src/foo/defs.gni": 'import("//build/config/clang/clang.gni")\n'
                + 'template("my_tpl") { group(target_name) {} }\n',
            }
            for rel, content in edits.items():
                with open(
                    os.path.join(tmpdir, rel), "w", encoding="utf-8"
                ) as f:
                    f.write(content)
            _git(tmpdir, "add", ".")
            _git(tmpdir, "commit", "-q", "-m", "[bazel_migration] macros")

            findings = _run_minimality(
                tmpdir, base, "src/foo", list(edits.keys())
            )
            self.assertEqual(
                {f["file"]: f["category"] for f in findings},
                {
                    "src/foo/defs.bzl": "macro_or_rule_modification_forbidden",
                    "src/foo/defs.gni": "macro_or_rule_modification_forbidden",
                },
            )

    def test_minor_list_edits_allowed_outside_build(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "sdk/atom_lists.bzl": HEADER
                    + '"""Atom lists."""\n\n'
                    + "ATOMS = [\n"
                    + '    "//sdk/lib/keep",\n'
                    + '    "//sdk/lib/old",\n'
                    + "]\n",
                    "sdk/tests.gni": "sdk_tests = [\n"
                    + '  "//sdk/lib/keep:tests",\n'
                    + "]\n",
                },
            )
            edits = {
                "sdk/atom_lists.bzl": HEADER
                + '"""Atom lists."""\n\n'
                + "ATOMS = [\n"
                + '    "//sdk/lib/keep",\n'
                + '    "//sdk/lib/new",\n'
                + "]\n",
                "sdk/tests.gni": "sdk_tests = [\n"
                + '  "//sdk/lib/keep:tests",\n'
                + '  "//sdk/lib/new:tests",\n'
                + "]\n",
            }
            for rel, content in edits.items():
                with open(
                    os.path.join(tmpdir, rel), "w", encoding="utf-8"
                ) as f:
                    f.write(content)
            _git(tmpdir, "add", ".")
            _git(tmpdir, "commit", "-q", "-m", "list edits")

            findings = _run_minimality(
                tmpdir, base, "sdk/lib/new", list(edits.keys())
            )
            self.assertEqual(findings, [])


class MigrationSanityBlockerAndTestChecksTest(unittest.TestCase):
    def _run_ms(self, repo: str, base_sha: str, target_dir: str) -> list[dict]:
        ms_sh = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "migration_sanity.sh"
        )
        env = {
            k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")
        }
        env.update(
            {
                "PLANTER_WORKDIR": repo,
                "PLANTER_TARGET_DIR": target_dir,
                "PLANTER_TARGET_DIRS": target_dir,
                "PLANTER_CHANGE_BASE": base_sha,
                "PLANTER_RESOLVED_CHANGE_BASE": base_sha,
            }
        )
        out = subprocess.run(
            ["bash", ms_sh],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertIn(out.returncode, (0, 1), out.stderr)
        return json.loads(out.stdout)

    def test_bind_suffix_target_matching_and_is_kernel_stripping(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            bind_gn = (
                'bind_library("fuchsia.foo") {\n  source = "foo.bind"\n}\n'
            )
            bind_bz = (
                HEADER
                + 'load("//build/bazel/rules/bind:defs.bzl", "fuchsia_bind_cc_library", "fuchsia_bind_library")\n\n'
                + 'fuchsia_bind_library(\n    name = "fuchsia.foo",\n    srcs = ["foo.bind"],\n)\n\n'
                + 'fuchsia_bind_cc_library(\n    name = "fuchsia.foo_cc",\n    library = ":fuchsia.foo",\n)\n'
            )
            khelper_gn = (
                'source_set("khelper") {\n'
                '  sources = [ "k.cc" ]\n'
                "  if (is_kernel) {\n"
                '    deps = [ "//zircon/kernel/lib/ktl" ]\n'
                "  }\n"
                "}\n\n"
                "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
            )
            khelper_bz = (
                HEADER
                + 'load("@rules_cc//cc:defs.bzl", "cc_library")\n\n'
                + 'cc_library(\n    name = "khelper",\n    srcs = ["k.cc"],\n)\n'
            )
            base = _init_repo(
                tmpdir,
                {
                    "src/devices/bind/fuchsia.foo/BUILD.gn": bind_gn,
                    "src/devices/bind/fuchsia.foo/BUILD.bazel": bind_bz,
                    "src/lib/khelper/BUILD.gn": khelper_gn,
                    "src/lib/khelper/BUILD.bazel": khelper_bz,
                    "zircon/kernel/lib/ktl/BUILD.gn": 'source_set("ktl") {}\n',
                    "src/drv_rs/BUILD.gn": (
                        'rustc_library("drv_rs") {\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        "  deps = [\n"
                        '    "//src/devices/bind/fuchsia.foo:fuchsia.foo_rust",\n'
                        '    "//src/lib/khelper",\n'
                        "  ]\n"
                        "}\n"
                    ),
                    "src/drv_cc/BUILD.gn": (
                        'source_set("drv_cc") {\n'
                        '  sources = [ "drv.cc" ]\n'
                        "  deps = [\n"
                        '    "//src/devices/bind/fuchsia.foo:fuchsia.foo_cpp",\n'
                        '    "//src/lib/khelper",\n'
                        "  ]\n"
                        "}\n"
                    ),
                },
            )
            rs_findings = self._run_ms(tmpdir, base, "src/drv_rs")
            blocked = [
                f
                for f in rs_findings
                if f["category"] == "target_blocked_by_unmigrated_deps"
            ]
            self.assertEqual(len(blocked), 1, rs_findings)
            self.assertIn(
                "//src/devices/bind/fuchsia.foo", blocked[0]["message"]
            )
            self.assertNotIn("khelper", blocked[0]["message"])

            cc_findings = self._run_ms(tmpdir, base, "src/drv_cc")
            cc_cats = [f["category"] for f in cc_findings]
            self.assertIn("missing_target_bazel_file", cc_cats, cc_findings)
            self.assertNotIn(
                "target_blocked_by_unmigrated_deps", cc_cats, cc_findings
            )

    def test_pre_sentinel_test_package_companion_dep_and_blocker_remediation(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/comp/BUILD.gn": (
                        "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                        'fuchsia_component("comp") {}\n'
                    ),
                    "src/comp/BUILD.bazel": (
                        HEADER
                        + 'load("//build/bazel/rules/fuchsia:defs.bzl", "fuchsia_component")\n\n'
                        + 'fuchsia_component(\n    name = "comp",\n    manifest = "meta/comp.cml",\n)\n'
                    ),
                    "src/unmig/BUILD.gn": 'rustc_library("unmig") {\n  sources = ["src/lib.rs"]\n}\n',
                    "src/pkg_err/BUILD.gn": (
                        'rustc_library("pkg_err") {\n  sources = [ "src/lib.rs" ]\n}\n'
                        'rustc_test("pkg_err_test") {\n  sources = [ "src/lib.rs" ]\n}\n'
                        'fuchsia_unittest_package("pkg_err-tests") {\n'
                        '  deps = [ ":pkg_err_test", "//src/comp:comp" ]\n'
                        "}\n"
                    ),
                    "src/pkg_err/src/lib.rs": "",
                    "src/pkg_blk/BUILD.gn": (
                        'rustc_library("pkg_blk") {\n  sources = [ "src/lib.rs" ]\n}\n'
                        'rustc_test("pkg_blk_test") {\n'
                        '  sources = [ "src/lib.rs" ]\n'
                        '  deps = [ "//src/unmig" ]\n'
                        "}\n"
                        'fuchsia_unittest_package("pkg_blk-tests") {\n'
                        '  deps = [ ":pkg_blk_test", "//src/comp:comp" ]\n'
                        "}\n"
                    ),
                    "src/pkg_blk/src/lib.rs": "",
                },
            )
            # Migrate library only in src/pkg_err and src/pkg_blk, leaving tests above sentinel.
            for pkg in ("pkg_err", "pkg_blk"):
                with open(
                    os.path.join(tmpdir, f"src/{pkg}/BUILD.bazel"),
                    "w",
                    encoding="utf-8",
                ) as f:
                    f.write(
                        HEADER
                        + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n\n'
                        + f'rustc_library(\n    name = "{pkg}",\n    srcs = ["src/lib.rs"],\n    edition = "2024",\n)\n'
                    )
            with open(
                os.path.join(tmpdir, "src/pkg_err/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    'rustc_test("pkg_err_test") {\n  sources = [ "src/lib.rs" ]\n}\n'
                    'fuchsia_unittest_package("pkg_err-tests") {\n'
                    '  deps = [ ":pkg_err_test", "//src/comp:comp" ]\n'
                    "}\n\n"
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("pkg_err") {\n  sources = [ "src/lib.rs" ]\n}\n'
                )
            with open(
                os.path.join(tmpdir, "src/pkg_blk/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    'rustc_test("pkg_blk_test") {\n'
                    '  sources = [ "src/lib.rs" ]\n'
                    '  deps = [ "//src/unmig" ]\n'
                    "}\n"
                    'fuchsia_unittest_package("pkg_blk-tests") {\n'
                    '  deps = [ ":pkg_blk_test", "//src/comp:comp" ]\n'
                    "}\n\n"
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    'rustc_library("pkg_blk") {\n  sources = [ "src/lib.rs" ]\n}\n'
                )

            err_findings = self._run_ms(tmpdir, base, "src/pkg_err")
            pkg_not_mig = [
                f
                for f in err_findings
                if f["category"] == "gn_test_package_not_migrated"
                and f["file"] == "src/pkg_err/BUILD.gn"
            ]
            self.assertEqual(len(pkg_not_mig), 1, err_findings)
            self.assertEqual(pkg_not_mig[0]["severity"], "error")

            blk_findings = self._run_ms(tmpdir, base, "src/pkg_blk")
            blk_pkg = [
                f
                for f in blk_findings
                if f["category"] == "gn_test_package_not_migrated"
                and f["file"] == "src/pkg_blk/BUILD.gn"
            ]
            self.assertEqual(len(blk_pkg), 1, blk_findings)
            self.assertEqual(blk_pkg[0]["severity"], "error")
            self.assertIn("//src/unmig", blk_pkg[0]["message"])
            self.assertIn("disputed_findings: []", blk_pkg[0]["remediation"])
            self.assertIn(
                "never disputing `empty_migration_diff`",
                blk_pkg[0]["remediation"],
            )


class PythonRulesAndDataPackageTest(unittest.TestCase):
    def _run_check(
        self,
        script_name: str,
        repo: str,
        base_sha: str,
        target_dir: str,
        changed_files: list[str],
    ) -> list[dict]:
        script_path = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), script_name
        )
        env = {
            k: v for k, v in os.environ.items() if not k.startswith("PLANTER_")
        }
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
            ["bash", script_path],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        return json.loads(out.stdout)

    def test_py_zipapp_binary_and_py_binary_host_tool_host_constraints(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "tools/pytool/BUILD.gn": (
                        "if (is_host) {\n"
                        '  python_binary("my_zipapp") {\n'
                        '    main_source = "main.py"\n'
                        "  }\n"
                        '  python_binary("my_legacy_tool") {\n'
                        '    main_source = "legacy.py"\n'
                        "  }\n"
                        '  python_binary("skip_zipapp") {\n'
                        '    main_source = "skip.py"\n'
                        "  }\n"
                        "}\n"
                        'python_binary("unguarded_zipapp") {\n'
                        '  main_source = "unguarded.py"\n'
                        "}\n"
                    ),
                    "tools/pytool/main.py": "def main():\n  return 0\n",
                    "tools/pytool/legacy.py": "def main():\n  return 0\n",
                    "tools/pytool/skip.py": "def main():\n  return 0\n",
                    "tools/pytool/unguarded.py": "def main():\n  return 0\n",
                },
            )
            # Keep a dual-build BUILD.gn: convertible Python rules there are only
            # host-only in GN when guarded, but py_zipapp_binary never syncs to GN.
            with open(
                os.path.join(tmpdir, "tools/pytool/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    'import("//build/bazel/bazel_py_host_tool.gni")\n\n'
                    "if (is_host) {\n"
                    '  bazel_py_host_tool("my_zipapp") {\n'
                    '    bazel_target = ":my_zipapp"\n'
                    "  }\n"
                    "}\n\n"
                    "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                )
            bz_rel = "tools/pytool/BUILD.bazel"
            # Valid py_zipapp_binary (omits target_compatible_with because rule defaults it)
            # and py_binary_host_tool omitting required target_compatible_with.
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/host:defs.bzl", "py_binary_host_tool")\n'
                    + 'load("//build/bazel/rules/python:py_zipapp_binary.bzl", "py_zipapp_binary")\n\n'
                    + "py_zipapp_binary(\n"
                    + '    name = "my_zipapp",\n'
                    + '    srcs = ["main.py"],\n'
                    + '    main = "main.py",\n'
                    + ")\n\n"
                    + "py_binary_host_tool(\n"
                    + '    name = "my_legacy_tool",\n'
                    + '    srcs = ["legacy.py"],\n'
                    + '    main = "legacy.py",\n'
                    + ")\n"
                )
            missing_findings = _run_minimality(
                tmpdir, base, "tools/pytool", [bz_rel]
            )
            missing = [
                f
                for f in missing_findings
                if f["category"] == "missing_host_os_constraints"
            ]
            self.assertEqual(len(missing), 1, missing_findings)
            self.assertIn("my_legacy_tool", missing[0]["message"])

            # Stale TODOs on py_zipapp_binary and py_binary_host_tool are flagged
            # without bazel2gn advice, including for an unguarded GN target in a
            # dual-build package, while `# @bazel2gn:skip` alone is not flagged.
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                    + 'load("//build/bazel/rules/host:defs.bzl", "py_binary_host_tool")\n'
                    + 'load("//build/bazel/rules/python:py_zipapp_binary.bzl", "py_zipapp_binary")\n\n'
                    + "# TODO: Verify whether my_zipapp should use target_compatible_with\n"
                    + "py_zipapp_binary(\n"
                    + '    name = "my_zipapp",\n'
                    + '    srcs = ["main.py"],\n'
                    + '    main = "main.py",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n\n"
                    + "# TODO: Verify whether my_legacy_tool should use target_compatible_with\n"
                    + "py_binary_host_tool(\n"
                    + '    name = "my_legacy_tool",\n'
                    + '    srcs = ["legacy.py"],\n'
                    + '    main = "legacy.py",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n\n"
                    + "# TODO: Verify whether unguarded_zipapp should use target_compatible_with\n"
                    + "py_zipapp_binary(\n"
                    + '    name = "unguarded_zipapp",\n'
                    + '    srcs = ["unguarded.py"],\n'
                    + '    main = "unguarded.py",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n\n"
                    + "py_zipapp_binary(\n"
                    + '    name = "skip_zipapp",\n'
                    + '    srcs = ["skip.py"],\n'
                    + '    main = "skip.py",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,  # @bazel2gn:skip\n"
                    + ")\n"
                )
            stale_todo_findings = _run_minimality(
                tmpdir, base, "tools/pytool", [bz_rel]
            )
            skipped = {
                f["message"].split("'")[1]: f
                for f in stale_todo_findings
                if f["category"] == "skipped_host_os_constraints"
            }
            self.assertEqual(
                sorted(skipped),
                ["my_legacy_tool", "my_zipapp", "unguarded_zipapp"],
                stale_todo_findings,
            )
            for name in ("my_zipapp", "unguarded_zipapp"):
                self.assertIn(
                    "`py_zipapp_binary` defaults", skipped[name]["message"]
                )
            self.assertIn(
                "`py_binary_host_tool` targets are always host-only",
                skipped["my_legacy_tool"]["message"],
            )
            for finding in skipped.values():
                self.assertNotIn("bazel2gn", finding["remediation"])

    def test_py_library_data_package_rules_and_zipapp(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "tools/pydata/BUILD.gn": (
                        'python_library("lib") {\n'
                        '  sources = [ "pkg/__init__.py", "pkg/mod.py" ]\n'
                        '  data_package_name = "data"\n'
                        '  data_sources = [ "files/item.txt" ]\n'
                        "}\n"
                        'python_binary("tool_ok") {\n'
                        '  main_source = "tool.py"\n'
                        '  data_package_name = "tool_data"\n'
                        '  data_sources = [ "files/item.txt" ]\n'
                        "}\n"
                        'python_binary("tool_bad") {\n'
                        '  main_source = "tool.py"\n'
                        '  data_package_name = "tool_data"\n'
                        '  data_sources = [ "files/item.txt" ]\n'
                        "}\n"
                        'python_binary("host_bin") {\n'
                        '  main_source = "tool.py"\n'
                        '  data_package_name = "host_data"\n'
                        '  data_sources = [ "files/item.txt" ]\n'
                        "}\n"
                        'python_binary("host_bad") {\n'
                        '  main_source = "tool.py"\n'
                        '  data_package_name = "bad_data"\n'
                        '  data_sources = [ "files/item.txt" ]\n'
                        "}\n"
                    ),
                    "tools/pydata/pkg/__init__.py": "",
                    "tools/pydata/pkg/mod.py": "",
                    "tools/pydata/tool.py": "def main():\n  return 0\n",
                    "tools/pydata/files/item.txt": "hi\n",
                },
            )
            os.remove(os.path.join(tmpdir, "tools/pydata/BUILD.gn"))
            bz_rel = "tools/pydata/BUILD.bazel"
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("@bazel_skylib//rules:copy_file.bzl", "copy_file")\n'
                    + 'load("@bazel_skylib//rules:write_file.bzl", "write_file")\n'
                    + 'load("@rules_python//python:py_library.bzl", "py_library")\n'
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n'
                    + 'load("//build/bazel/rules/host:defs.bzl", "py_binary_host_tool")\n'
                    + 'load("//build/bazel/rules/python:py_zipapp_binary.bzl", "py_zipapp_binary")\n\n'
                    + "py_library(\n"
                    + '    name = "lib",\n'
                    + '    srcs = ["pkg/__init__.py", "pkg/mod.py"],\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n\n"
                    + "py_zipapp_binary(\n"
                    + '    name = "tool_ok",\n'
                    + '    srcs = ["tool.py"],\n'
                    + '    data_package_name = "tool_data",\n'
                    + '    data_sources = ["files/item.txt"],\n'
                    + '    main = "tool.py",\n'
                    + ")\n\n"
                    + "py_zipapp_binary(\n"
                    + '    name = "tool_bad",\n'
                    + '    srcs = ["tool.py"],\n'
                    + '    main = "tool.py",\n'
                    + ")\n\n"
                    + "write_file(\n"
                    + '    name = "host_data_init",\n'
                    + '    out = "host_data/__init__.py",\n'
                    + '    content = [""],\n'
                    + ")\n\n"
                    + "copy_file(\n"
                    + '    name = "host_data_item",\n'
                    + '    src = "files/item.txt",\n'
                    + '    out = "host_data/item.txt",\n'
                    + ")\n\n"
                    + "py_binary_host_tool(\n"
                    + '    name = "host_bin",\n'
                    + '    srcs = ["pkg/__init__.py", "tool.py", ":host_data_init"],\n'
                    + '    data = [":host_data_item"],\n'
                    + '    main = "tool.py",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n\n"
                    + "py_binary_host_tool(\n"
                    + '    name = "host_bad",\n'
                    + '    srcs = ["tool.py"],\n'
                    + '    main = "tool.py",\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n"
                )
            findings = self._run_check(
                "py_library_data_package.sh",
                tmpdir,
                base,
                "tools/pydata",
                [bz_rel],
            )
            got = sorted(
                (f["category"], f["message"].split(")", 1)[0] + ")")
                for f in findings
            )
            self.assertEqual(
                got,
                [
                    (
                        "py_library_missing_data_package_init",
                        'py_binary_host_tool(name = "host_bad")',
                    ),
                    (
                        "py_library_missing_data_package_init",
                        'py_library(name = "lib")',
                    ),
                    (
                        "py_library_missing_data_source",
                        'py_binary_host_tool(name = "host_bad")',
                    ),
                    (
                        "py_library_missing_data_source",
                        'py_library(name = "lib")',
                    ),
                    (
                        "py_zipapp_missing_data_package",
                        'py_zipapp_binary(name = "tool_bad")',
                    ),
                    (
                        "py_zipapp_missing_data_source",
                        'py_zipapp_binary(name = "tool_bad")',
                    ),
                ],
                findings,
            )
            lib_src_err = next(
                f
                for f in findings
                if f["category"] == "py_library_missing_data_source"
                and f["message"].startswith('py_library(name = "lib")')
            )
            self.assertIn(
                'importlib.resources.files("data")', lib_src_err["message"]
            )

    def test_py_zipapp_binary_data_sources_label_forms(self):
        gn_binaries = (
            ("by_colon_label", '"files/item.txt"'),
            ("by_full_label", '"files/item.txt"'),
            ("by_custom_rule", '"$target_gen_dir/gen.json"'),
            ("by_partial_list", '"files/item.txt", "files/other.txt"'),
            ("missing_one", '"files/item.txt", "files/other.txt"'),
        )
        bazel_data_sources = (
            ("by_colon_label", '[":files/item.txt"]'),
            ("by_full_label", '["//tools/pyzip:files/item.txt"]'),
            ("by_custom_rule", '[":gen"]'),
            ("by_partial_list", '["files/item.txt"] + EXTRA_DATA'),
            ("missing_one", '[":files/item.txt"]'),
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "tools/pyzip/BUILD.gn": "".join(
                        f'python_binary("{name}") {{\n'
                        '  main_source = "tool.py"\n'
                        '  data_package_name = "tool_data"\n'
                        f"  data_sources = [ {sources} ]\n"
                        "}\n"
                        for name, sources in gn_binaries
                    ),
                    "tools/pyzip/tool.py": "def main():\n  return 0\n",
                    "tools/pyzip/files/item.txt": "hi\n",
                    "tools/pyzip/files/other.txt": "hi\n",
                },
            )
            os.remove(os.path.join(tmpdir, "tools/pyzip/BUILD.gn"))
            bz_rel = "tools/pyzip/BUILD.bazel"
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/python:py_zipapp_binary.bzl", "py_zipapp_binary")\n'
                    + 'load("//tools/pyzip:defs.bzl", "EXTRA_DATA", "gen_json")\n\n'
                    + 'gen_json(name = "gen")\n\n'
                    + "".join(
                        "py_zipapp_binary(\n"
                        f'    name = "{name}",\n'
                        '    srcs = ["tool.py"],\n'
                        '    data_package_name = "tool_data",\n'
                        f"    data_sources = {sources},\n"
                        '    main = "tool.py",\n'
                        ")\n\n"
                        for name, sources in bazel_data_sources
                    )
                )
            findings = self._run_check(
                "py_library_data_package.sh",
                tmpdir,
                base,
                "tools/pyzip",
                [bz_rel],
            )
            self.assertEqual(
                [(f["category"], f["message"].split('"')[1]) for f in findings],
                [("py_zipapp_missing_data_source", "missing_one")],
                findings,
            )
            self.assertIn("`other.txt`", findings[0]["message"])
            self.assertNotIn("`item.txt`", findings[0]["message"])

    def test_rustc_staticlib_raw_public_annotation(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/foo/BUILD.gn": 'rustc_staticlib("c_lib") {}\n',
                    "src/foo/BUILD.bazel": (
                        HEADER
                        + 'load("//build/bazel/rules/rust:rustc_staticlib.bzl", "rustc_staticlib")\n\n'
                        + "rustc_staticlib(\n"
                        + '    name = "kept_existing",\n'
                        + '    srcs = ["src/kept.rs"],\n'
                        + '    # @bazel2gn:raw_attribute:public = ["kept.h"]\n'
                        + "    # @bazel2gn:skip\n"
                        + '    compile_data = ["kept.h"],\n'
                        + ")\n"
                    ),
                },
            )
            bz_rel = "src/foo/BUILD.bazel"
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:rustc_staticlib.bzl", "rustc_staticlib")\n\n'
                    + "rustc_staticlib(\n"
                    + '    name = "kept_existing",\n'
                    + '    srcs = ["src/kept.rs"],\n'
                    + '    # @bazel2gn:raw_attribute:public = ["kept.h"]\n'
                    + "    # @bazel2gn:skip\n"
                    + '    compile_data = ["kept.h"],\n'
                    + ")\n\n"
                    + "rustc_staticlib(\n"
                    + '    name = "c_lib",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    # @bazel2gn:raw_attribute:public = ["c_lib.h"]\n'
                    + "    # @bazel2gn:skip\n"
                    + '    compile_data = ["c_lib.h"],\n'
                    + ")\n"
                )
            findings = self._run_check(
                "bazel_minimality.sh",
                tmpdir,
                base,
                "src/foo",
                [bz_rel],
            )
            hits = [
                f
                for f in findings
                if f["category"] == "rustc_staticlib_raw_public_annotation"
            ]
            self.assertEqual(len(hits), 1, hits)
            self.assertIn("c_lib_headers", hits[0]["remediation"])

    def test_gn_dep_parity_preserves_sibling_shared_and_static_deps(self):
        dep_gn = (
            'source_set("processing") {}\n'
            'shared_library("prebuilt_coefficient_tables") {}\n'
        )
        dep_bz = (
            HEADER
            + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
            + 'fx_cc_library(\n    name = "processing",\n    srcs = ["a.cc"],\n)\n\n'
            + "# @bazel2gn:skip\n"
            + 'fx_cc_library(\n    name = "prebuilt_coefficient_tables",\n    srcs = ["b.cc"],\n)\n'
        )
        consumer_gn = (
            'executable("mixer_bin") {\n'
            "  deps = [\n"
            '    "//src/dep:prebuilt_coefficient_tables",\n'
            '    "//src/dep:processing",\n'
            "  ]\n"
            "}\n"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/dep/BUILD.gn": dep_gn,
                    "src/dep/BUILD.bazel": dep_bz,
                    "src/consumer/BUILD.gn": consumer_gn,
                },
            )
            os.remove(os.path.join(tmpdir, "src/consumer/BUILD.gn"))
            bz_rel = "src/consumer/BUILD.bazel"
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_binary")\n\n'
                    + "fx_cc_binary(\n"
                    + '    name = "mixer_bin",\n'
                    + "    deps = [\n"
                    + '        "//src/dep:prebuilt_coefficient_tables",\n'
                    + '        "//src/dep:processing",\n'
                    + "    ],\n"
                    + ")\n"
                )
            findings = self._run_check(
                "gn_dep_parity.sh",
                tmpdir,
                base,
                "src/consumer",
                [bz_rel, "src/consumer/BUILD.gn"],
            )
            subs = [
                f for f in findings if f["category"] == "dep_label_substituted"
            ]
            self.assertEqual(subs, [], subs)

            # Dropping the shared_library dep while keeping the sibling static dep still flags.
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_binary")\n\n'
                    + "fx_cc_binary(\n"
                    + '    name = "mixer_bin",\n'
                    + '    deps = ["//src/dep:processing"],\n'
                    + ")\n"
                )
            findings2 = self._run_check(
                "gn_dep_parity.sh",
                tmpdir,
                base,
                "src/consumer",
                [bz_rel, "src/consumer/BUILD.gn"],
            )
            subs2 = [
                f for f in findings2 if f["category"] == "dep_label_substituted"
            ]
            self.assertEqual(len(subs2), 1, subs2)

    def test_redundant_panic_abort_rustc_flags(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/foo/BUILD.gn": (
                        'rustc_staticlib("wlansoftmac-c") {\n  sources = ["src/lib.rs"]\n}\n'
                        'rustc_library("new_lib") {\n  sources = ["src/a.rs"]\n}\n'
                    ),
                    "src/foo/BUILD.bazel": (
                        HEADER
                        + 'load("@rules_rust//rust:defs.bzl", "rust_static_library")\n'
                        + 'load("//build/bazel/rules/rust:rustc_staticlib.bzl", "rustc_staticlib")\n\n'
                        + "rust_static_library(\n"
                        + '    name = "wlansoftmac-c",\n'
                        + '    srcs = ["src/lib.rs"],\n'
                        + '    rustc_flags = ["-Cpanic=abort"],\n'
                        + ")\n\n"
                        + "rustc_staticlib(\n"
                        + '    name = "untouched_existing",\n'
                        + '    srcs = ["src/u.rs"],\n'
                        + '    rustc_flags = ["-Cpanic=abort"],\n'
                        + ")\n"
                    ),
                },
            )
            bz_rel = "src/foo/BUILD.bazel"
            with open(os.path.join(tmpdir, bz_rel), "w", encoding="utf-8") as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/rust:rustc_staticlib.bzl", "rustc_staticlib")\n'
                    + 'load("//build/bazel/rules/rust:rustc_library.bzl", "rustc_library")\n'
                    + 'load("//build/bazel/rules/host:defs.bzl", "rustc_binary_host_tool")\n'
                    + 'load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")\n\n'
                    + "rustc_staticlib(\n"
                    + '    name = "wlansoftmac-c",\n'
                    + '    srcs = ["src/lib.rs"],\n'
                    + '    rustc_flags = ["-Cpanic=abort"],\n'
                    + ")\n\n"
                    + "rustc_staticlib(\n"
                    + '    name = "untouched_existing",\n'
                    + '    srcs = ["src/u.rs"],\n'
                    + '    rustc_flags = ["-Cpanic=abort"],\n'
                    + ")\n\n"
                    + "rustc_library(\n"
                    + '    name = "new_lib",\n'
                    + '    srcs = ["src/a.rs"],\n'
                    + '    rustc_flags = ["-Cpanic=abort"],\n'
                    + ")\n\n"
                    + "rustc_binary_host_tool(\n"
                    + '    name = "host_tool",\n'
                    + '    srcs = ["src/h.rs"],\n'
                    + '    rustc_flags = ["-Cpanic=abort"],\n'
                    + ")\n\n"
                    + "rustc_library(\n"
                    + '    name = "host_lib",\n'
                    + '    srcs = ["src/hl.rs"],\n'
                    + '    rustc_flags = ["-Cpanic=abort"],\n'
                    + "    target_compatible_with = HOST_OS_CONSTRAINTS,\n"
                    + ")\n"
                )
            findings = self._run_check(
                "bazel_minimality.sh",
                tmpdir,
                base,
                "src/foo",
                [bz_rel],
            )
            panic_msgs = [
                f["message"]
                for f in findings
                if f["category"] == "redundant_default_attribute"
                and "-Cpanic=abort" in f["message"]
            ]
            self.assertEqual(len(panic_msgs), 2, panic_msgs)
            self.assertTrue(
                any("wlansoftmac-c" in m for m in panic_msgs), panic_msgs
            )
            self.assertTrue(any("new_lib" in m for m in panic_msgs), panic_msgs)

    def test_header_only_dual_built_source_set_alwayslink_allowed(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/dual/BUILD.gn": (
                        HEADER
                        + 'source_set("constants") {\n'
                        + '  sources = [ "constants.h" ]\n'
                        + "}\n"
                    ),
                    "src/dual/constants.h": "// header\n",
                    "src/case1/BUILD.gn": (
                        HEADER
                        + 'source_set("headers") {\n'
                        + '  sources = [ "headers.h" ]\n'
                        + "}\n"
                    ),
                    "src/case1/headers.h": "// header\n",
                },
            )
            with open(
                os.path.join(tmpdir, "src/dual/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                    + 'fx_cc_library(\n    name = "constants",\n'
                    + '    hdrs = ["constants.h"],\n    alwayslink = True,\n)\n'
                )
            with open(
                os.path.join(tmpdir, "src/dual/BUILD.gn"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                    + 'source_set("constants") {\n  public = [ "constants.h" ]\n}\n'
                )
            findings_dual = self._run_check(
                "gn_target_type_parity.sh",
                tmpdir,
                base,
                "src/dual",
                ["src/dual/BUILD.bazel", "src/dual/BUILD.gn"],
            )
            self.assertEqual(
                [
                    f
                    for f in findings_dual
                    if f["category"] == "header_only_library_with_alwayslink"
                ],
                [],
                findings_dual,
            )

            with open(
                os.path.join(tmpdir, "src/case1/BUILD.bazel"),
                "w",
                encoding="utf-8",
            ) as f:
                f.write(
                    HEADER
                    + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                    + 'fx_cc_library(\n    name = "headers",\n'
                    + '    hdrs = ["headers.h"],\n    alwayslink = True,\n)\n'
                )
            _git(tmpdir, "rm", "-q", "src/case1/BUILD.gn")
            findings_case1 = self._run_check(
                "gn_target_type_parity.sh",
                tmpdir,
                base,
                "src/case1",
                ["src/case1/BUILD.bazel", "src/case1/BUILD.gn"],
            )
            hits_case1 = [
                f
                for f in findings_case1
                if f["category"] == "header_only_library_with_alwayslink"
            ]
            self.assertEqual(len(hits_case1), 1, findings_case1)


if __name__ == "__main__":
    unittest.main()
