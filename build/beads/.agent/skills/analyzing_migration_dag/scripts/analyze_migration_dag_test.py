#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for analyze_migration_dag.py."""

import pathlib
import subprocess
import sys
import tempfile
import unittest

import analyze_migration_dag as amd


def _write_build_file(root: pathlib.Path, rel_path: str, content: str) -> None:
    path = root / rel_path
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)


class AnalyzeMigrationDagTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self._tmp.name)
        amd.load_third_party_map.cache_clear()
        amd.parse_build_gn_targets_and_tests.cache_clear()
        amd._read_clean_bazel_file.cache_clear()

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def test_kernel_package_preserves_intra_kernel_deps_and_flags_kernel_rust_mod(
        self,
    ) -> None:
        _write_build_file(
            self.root,
            "zircon/kernel/lib/debug/BUILD.gn",
            'source_set("debug") {}\n',
        )
        _write_build_file(
            self.root,
            "zircon/kernel/dev/pdev/power/BUILD.gn",
            """
            source_set("power") {
              deps = [
                ":power-rs",
                "//zircon/kernel/lib/debug",
                "//zircon/kernel/switch:asan",
              ]
            }
            kernel_rust_mod("power-rs") {
              deps = [ "//zircon/kernel/lib/debug" ]
            }
            """,
        )

        (
            edges,
            _gn_info,
            statuses,
            _depths,
            _truncated,
        ) = amd.build_full_dag_with_tests(
            self.root, ["//zircon/kernel/dev/pdev/power:power"]
        )

        self.assertIn(
            "//zircon/kernel/lib/debug:debug",
            edges["//zircon/kernel/dev/pdev/power:power"],
        )
        self.assertNotIn(
            "//zircon/kernel/switch:asan",
            edges["//zircon/kernel/dev/pdev/power:power"],
        )
        self.assertEqual(
            statuses["//zircon/kernel/dev/pdev/power:power-rs"][0],
            "HARD_BLOCKER",
        )
        self.assertEqual(
            statuses["//zircon/kernel/lib/debug:debug"][0],
            "UNMIGRATED_IN_SCOPE",
        )

    def test_non_kernel_package_ignores_is_kernel_zircon_deps(self) -> None:
        _write_build_file(
            self.root,
            "zircon/kernel/lib/ktl/BUILD.gn",
            'source_set("ktl") {}\n',
        )
        _write_build_file(
            self.root,
            "sdk/lib/fit/BUILD.gn",
            """
            source_set("fit") {
              if (is_kernel) {
                deps = [ "//zircon/kernel/lib/ktl" ]
              }
            }
            """,
        )

        (
            edges,
            _gn_info,
            statuses,
            _depths,
            _truncated,
        ) = amd.build_full_dag_with_tests(self.root, ["//sdk/lib/fit:fit"])

        self.assertEqual(edges["//sdk/lib/fit:fit"], set())
        self.assertNotIn("//zircon/kernel/lib/ktl:ktl", statuses)

    def test_bazel_wrapper_templates_and_toolchain_qualified_labels(
        self,
    ) -> None:
        _write_build_file(
            self.root,
            "src/sys/pkg/BUILD.gn",
            'bazel_fuchsia_package("pkg") { bazel_target = "//src/sys/pkg:pkg" }\n',
        )
        _write_build_file(
            self.root,
            "tools/helper/BUILD.gn",
            'bazel_host_tool("helper") { bazel_target = "//tools/helper:helper" }\n',
        )
        _write_build_file(
            self.root,
            "src/consumer/BUILD.gn",
            """
            source_set("consumer") {
              deps = [
                "//src/sys/pkg:pkg",
                "//tools/helper:helper($host_toolchain)",
                "//src/$dynamic_dir:ignored",
              ]
            }
            """,
        )

        (
            edges,
            _gn_info,
            statuses,
            _depths,
            _truncated,
        ) = amd.build_full_dag_with_tests(
            self.root, ["//src/consumer:consumer"]
        )

        self.assertEqual(
            edges["//src/consumer:consumer"],
            {"//src/sys/pkg:pkg", "//tools/helper:helper"},
        )
        self.assertEqual(statuses["//src/sys/pkg:pkg"][0], "MIGRATED_READY")
        self.assertEqual(statuses["//tools/helper:helper"][0], "MIGRATED_READY")

    def test_kernel_crate_templates_are_hard_blockers(self) -> None:
        _write_build_file(
            self.root,
            "zircon/kernel/lib/efi/BUILD.gn",
            """
            kernel_bindgen_crate("efi-bindings") {
              deps = [ "//third_party/rust_crates:zerocopy" ]
            }
            kernel_rust_crate("efi-rs") {
              deps = [ ":efi-bindings" ]
            }
            """,
        )

        (
            edges,
            _gn_info,
            statuses,
            _depths,
            _truncated,
        ) = amd.build_full_dag_with_tests(
            self.root, ["//zircon/kernel/lib/efi:efi-rs"]
        )

        self.assertIn(
            "//zircon/kernel/lib/efi:efi-bindings",
            edges["//zircon/kernel/lib/efi:efi-rs"],
        )
        self.assertEqual(
            statuses["//zircon/kernel/lib/efi:efi-rs"][0], "HARD_BLOCKER"
        )
        self.assertIn(
            "kernel_rust_crate.gni",
            statuses["//zircon/kernel/lib/efi:efi-rs"][1],
        )
        self.assertEqual(
            statuses["//zircon/kernel/lib/efi:efi-bindings"][0],
            "HARD_BLOCKER",
        )
        self.assertIn(
            "kernel_bindgen_crate.gni",
            statuses["//zircon/kernel/lib/efi:efi-bindings"][1],
        )

    def test_bazel_ffx_tool_wrapper_without_build_bazel_is_migrated(
        self,
    ) -> None:
        _write_build_file(
            self.root,
            "src/developer/ffx/tools/echo/BUILD.gn",
            'bazel_ffx_tool("ffx_echo") { output_name = "ffx-echo" }\n',
        )
        _write_build_file(
            self.root,
            "src/consumer/BUILD.gn",
            """
            group("consumer") {
              deps = [
                "//src/developer/ffx/tools/echo:ffx_echo($host_toolchain)",
              ]
            }
            """,
        )

        (
            edges,
            _gn_info,
            statuses,
            _depths,
            _truncated,
        ) = amd.build_full_dag_with_tests(
            self.root, ["//src/consumer:consumer"]
        )

        self.assertEqual(
            edges["//src/consumer:consumer"],
            {"//src/developer/ffx/tools/echo:ffx_echo"},
        )
        status = statuses["//src/developer/ffx/tools/echo:ffx_echo"]
        self.assertEqual(status[0], "MIGRATED_READY")
        self.assertIn("bazel_ffx_tool", status[1])

    def test_build_gn_cli_skips_test_aggregator_groups(self) -> None:
        _write_build_file(
            self.root,
            "src/lib/foo/BUILD.gn",
            """
            source_set("foo") {}
            group("tests") {
              testonly = true
              deps = [ "child:tests" ]
            }
            group("kernel-tests") {
              testonly = true
              deps = [ "child:kernel-tests" ]
            }
            """,
        )
        _write_build_file(
            self.root,
            "src/lib/foo/child/BUILD.gn",
            """
            test("child_test") {}
            group("tests") { testonly = true deps = [ ":child_test" ] }
            group("kernel-tests") { testonly = true deps = [ ":child_test" ] }
            """,
        )

        script_path = pathlib.Path(amd.__file__).resolve()
        completed = subprocess.run(
            [
                sys.executable,
                str(script_path),
                "//src/lib/foo/BUILD.gn",
                "--fuchsia-dir",
                str(self.root),
            ],
            capture_output=True,
            text=True,
            check=True,
        )

        self.assertIn("//src/lib/foo:foo", completed.stdout)
        self.assertNotIn("child_test", completed.stdout)


if __name__ == "__main__":
    unittest.main()
