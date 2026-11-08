#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for planter_check_lib (and rdeps_lib)."""

import io
import json
import os
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest
from contextlib import redirect_stdout
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import planter_check_lib as pcl


class PlanterCheckLibTest(unittest.TestCase):
    def test_canonical_sets(self):
        self.assertIn("bundles/assembly", pcl.EXCLUDED_GLOBAL_DIRS)
        self.assertIn("third_party/rust_crates", pcl.EXCLUDED_GLOBAL_DIRS)
        self.assertIn("rustc_test", pcl.BAZEL_TEST_RULES)
        self.assertIn("fx_unittest_package", pcl.BAZEL_TEST_RULES)
        self.assertIn("template", pcl.GN_NON_TARGET_KEYWORDS)
        self.assertIn("declare_args", pcl.GN_NON_TARGET_KEYWORDS)
        self.assertEqual(pcl.NOT_TARGETS, pcl.GN_NON_TARGET_KEYWORDS)

    def test_emit_and_emit_json(self):
        findings = []
        with mock.patch.dict(os.environ, {"PLANTER_CHECK_NAME": "my_check"}):
            entry = pcl.emit(
                findings,
                category="test_cat",
                severity="warn",
                file="src/foo/BUILD.bazel",
                line=42,
                message="something is off",
                remediation="fix it",
            )
        self.assertEqual(len(findings), 1)
        self.assertEqual(entry["source"], "my_check")
        self.assertEqual(entry["category"], "test_cat")
        self.assertEqual(entry["severity"], "WARNING")
        self.assertEqual(entry["file"], "src/foo/BUILD.bazel")
        self.assertEqual(entry["line"], 42)

        pcl.emit(
            findings,
            "info_cat",
            "nit",
            "a/b",
            1,
            "m",
            "r",
            source="explicit_src",
        )
        self.assertEqual(findings[1]["source"], "explicit_src")
        self.assertEqual(findings[1]["severity"], "INFO")

        pcl.emit(findings, "err_cat", "error", "a/b", 2, "m", "r")
        self.assertEqual(findings[2]["severity"], "error")

        buf = io.StringIO()
        with redirect_stdout(buf):
            pcl.emit_json(findings)
        parsed = json.loads(buf.getvalue())
        self.assertEqual(len(parsed), 3)
        self.assertEqual(parsed[0]["category"], "test_cat")

    def test_normalize_target_dir(self):
        self.assertEqual(
            pcl.normalize_target_dir("//src/lib/foo/"), "src/lib/foo"
        )
        self.assertEqual(
            pcl.normalize_target_dir("./src/lib/foo"), "src/lib/foo"
        )
        self.assertEqual(pcl.normalize_target_dir("."), "")
        self.assertEqual(
            pcl.normalize_target_dir(
                "/work/tree/src/bar", workdir="/work/tree"
            ),
            "src/bar",
        )

    def test_strip_and_blank_comments(self):
        src = textwrap.dedent(
            """            # Top comment
            rustc_library("foo") { # inline comment
              name = "foo#not_a_comment"
              deps = [ "//a/b" ] # trailing
            }
        """
        )
        stripped = pcl.strip_comments(src)
        self.assertNotIn("Top comment", stripped)
        self.assertNotIn("inline comment", stripped)
        self.assertIn('"foo#not_a_comment"', stripped)
        self.assertEqual(stripped.count("\n"), src.count("\n"))

        blanked = pcl.blank_comments(src)
        self.assertEqual(len(blanked), len(src))
        self.assertEqual(blanked.count("\n"), src.count("\n"))
        self.assertIn('"foo#not_a_comment"', blanked)

    def test_split_sentinel(self):
        gn_with_sentinel = textwrap.dedent(
            """            import("//build/rust/rustc_library.gni")
            group("tests") {
              testonly = true
            }
            ## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##
            rustc_library("foo") {
              edition = "2021"
            }
        """
        )
        self.assertTrue(pcl.has_sentinel(gn_with_sentinel))
        above, below = pcl.split_sentinel(gn_with_sentinel)
        self.assertIn('group("tests")', above)
        self.assertNotIn('rustc_library("foo")', above)
        self.assertIn('rustc_library("foo")', below)

        gn_local_sentinel = (
            'group("a") {}\n#LOCAL_BAZEL_BUILD_SENTINEL\nsource_set("b") {}\n'
        )
        self.assertTrue(pcl.has_sentinel(gn_local_sentinel))
        above2, below2 = pcl.split_sentinel(gn_local_sentinel)
        self.assertIn('group("a")', above2)
        self.assertIn('source_set("b")', below2)

        gn_no_sentinel = 'group("only") {}\n'
        self.assertFalse(pcl.has_sentinel(gn_no_sentinel))
        above3, below3 = pcl.split_sentinel(gn_no_sentinel)
        self.assertEqual(above3, gn_no_sentinel)
        self.assertEqual(below3, "")

    def test_extract_gn_targets(self):
        gn_text = textwrap.dedent(
            """            import("//build/components.gni")
            # rustc_library("commented_out") {}
            template("my_tmpl") {
              source_set(target_name) {}
            }
            if (is_host) {
              rustc_binary("my_bin") {
                deps = [ ":lib" ]
              }
            }
            rustc_library("lib") {
              sources = [ "src/lib.rs" ]
            }
        """
        )
        targets = pcl.extract_gn_targets(gn_text)
        names = [(t["type"], t["name"], t["line"]) for t in targets]
        self.assertEqual(
            names,
            [
                ("rustc_binary", "my_bin", 7),
                ("rustc_library", "lib", 11),
            ],
        )
        with_tmpls = pcl.extract_gn_targets(gn_text, include_templates=True)
        self.assertEqual(len(with_tmpls), 3)
        self.assertEqual(with_tmpls[0]["name"], "my_tmpl")

    def test_extract_bazel_targets(self):
        bazel_text = textwrap.dedent(
            """            load("//build/bazel/rules/rust:defs.bzl", "rustc_library", "rustc_test")

            COMMON_VIS = ["//src/sys:__subpackages__"]

            rustc_library(
                name = "foo",
                srcs = ["src/lib.rs"],
                deps = ["//src/lib/bar"],
                visibility = COMMON_VIS + ["//src/diagnostics:__pkg__"],
            )

            # @bazel2gn:skip
            rustc_test(
                name = "foo_test",
                testonly = True,
                deps = [":foo"],
            )
        """
        )
        targets = pcl.extract_bazel_targets(bazel_text)
        self.assertEqual(len(targets), 2)
        t0, t1 = targets[0], targets[1]
        self.assertEqual(t0["name"], "foo")
        self.assertEqual(t0["rule"], "rustc_library")
        self.assertFalse(t0["has_bazel2gn_skip"])
        self.assertEqual(
            t0["visibility"],
            ["//src/sys:__subpackages__", "//src/diagnostics:__pkg__"],
        )
        self.assertEqual(t0["deps"], ["//src/lib/bar"])

        self.assertEqual(t1["name"], "foo_test")
        self.assertEqual(t1["rule"], "rustc_test")
        self.assertTrue(t1["testonly"])
        self.assertTrue(t1["has_bazel2gn_skip"])

    def test_read_review_diff_findings(self):
        with tempfile.NamedTemporaryFile(
            "w", suffix=".json", delete=False
        ) as tf:
            json.dump([{"category": "visibility_narrowed", "line": 10}], tf)
            path = tf.name
        try:
            with mock.patch.dict(
                os.environ, {"PLANTER_REVIEW_DIFF_JSON": path}
            ):
                res = pcl.read_review_diff_findings()
                self.assertEqual(len(res), 1)
                self.assertEqual(res[0]["category"], "visibility_narrowed")
            with mock.patch.dict(
                os.environ, {"PLANTER_REVIEW_DIFF_JSON": "/nonexistent"}
            ):
                self.assertEqual(pcl.read_review_diff_findings(), [])
        finally:
            os.unlink(path)

    def test_resolve_change_base_and_changed_files(self):
        with mock.patch.dict(
            os.environ,
            {
                "PLANTER_RESOLVED_CHANGE_BASE": "deadbeef",
                "PLANTER_CHANGED_FILES": "src/a/BUILD.bazel\nsrc/a/BUILD.gn\n",
            },
        ):
            self.assertEqual(pcl.resolve_change_base("/tmp"), "deadbeef")
            self.assertEqual(
                pcl.changed_files("/tmp"),
                ["src/a/BUILD.bazel", "src/a/BUILD.gn"],
            )

        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    tmpdir,
                    "config",
                    "user.email",
                    "test-user",
                ],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test"],
                check=True,
            )
            f1 = os.path.join(tmpdir, "BUILD.gn")
            with open(f1, "w") as f:
                f.write("# init\n")
            subprocess.run(["git", "-C", tmpdir, "add", "BUILD.gn"], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "initial"],
                check=True,
            )

            f2 = os.path.join(tmpdir, "BUILD.bazel")
            with open(f2, "w") as f:
                f.write("# migrated\n")
            subprocess.run(
                ["git", "-C", tmpdir, "add", "BUILD.bazel"], check=True
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    tmpdir,
                    "commit",
                    "-q",
                    "-m",
                    "[bazel2gn] Migrate pkg",
                ],
                check=True,
            )
            with open(os.path.join(tmpdir, "CoderReport1.json"), "w") as f:
                f.write("{}")

            env = {
                k: v
                for k, v in os.environ.items()
                if k
                not in (
                    "PLANTER_RESOLVED_CHANGE_BASE",
                    "PLANTER_CHANGED_FILES",
                    "PLANTER_CHANGE_BASE",
                    "PLANTER_TARGET_DIR",
                    "PLANTER_TARGET_DIRS",
                )
            }
            with mock.patch.dict(os.environ, env, clear=True):
                self.assertEqual(pcl.resolve_change_base(tmpdir), "HEAD~1")
                cfiles = pcl.changed_files(tmpdir)
                self.assertIn("BUILD.bazel", cfiles)
                self.assertNotIn("CoderReport1.json", cfiles)

                # Upstream [bazel2gn]/[bazel_migration] commits on origin/main
                # must not step change_base back to HEAD~1.
                subprocess.run(
                    [
                        "git",
                        "-C",
                        tmpdir,
                        "update-ref",
                        "refs/remotes/origin/main",
                        "HEAD",
                    ],
                    check=True,
                )
                self.assertEqual(pcl.resolve_change_base(tmpdir), "HEAD")

                # Stacked prerequisite migration commits outside target_dirs
                # must stay at HEAD while local uncommitted edits are in progress.
                subprocess.run(
                    [
                        "git",
                        "-C",
                        tmpdir,
                        "update-ref",
                        "refs/remotes/origin/main",
                        "HEAD~1",
                    ],
                    check=True,
                )
                os.makedirs(os.path.join(tmpdir, "src", "other"))
                self.assertEqual(
                    pcl.resolve_change_base(tmpdir, target_dirs=["src/other"]),
                    "HEAD",
                )
                self.assertEqual(pcl.resolve_change_base(tmpdir), "HEAD~1")

    def test_external_repo_test_blockers(self):
        """Shared external-repo detection used by migration_sanity and cq_reachability."""
        with tempfile.TemporaryDirectory() as tmpdir:

            def write(rel, text):
                path = os.path.join(tmpdir, rel)
                os.makedirs(os.path.dirname(path), exist_ok=True)
                with open(path, "w", encoding="utf-8") as f:
                    f.write(text)

            write(
                "build/bazel/toplevel.MODULE.bazel",
                'bazel_dep(name = "badmod", version = "1", repo_name = "rules_incompat")\n'
                'local_path_override(module_name = "badmod", path = "third_party/badmod")\n',
            )
            write(
                "third_party/badmod/BUILD.bazel",
                'cc_library(name = "x", srcs = ["x.cc"])\n',
            )
            write(
                "third_party/badmod/x.cc",
                "#if defined(__Fuchsia__)\n#include <fuchsia/io/cpp/fidl.h>\n#endif\n",
            )
            write(
                "build/tools/bazel2gn/third_party_target_map.json",
                json.dumps({"@noinc//:lib": "//third_party/noinc:lib"}),
            )
            write(
                "third_party/noinc/src/BUILD.bazel",
                'cc_library(name = "lib")\n',
            )
            write("src/pkg/a.cc", "#include <noinc/api.h>\n")
            bazel_text = 'fx_cc_test_binary(name = "t", deps = ["@rules_incompat//:x", "@noinc//:lib"])\n'

            self.assertEqual(
                pcl.fuchsia_incompatible_ext_repos(tmpdir),
                {"badmod": "badmod", "rules_incompat": "badmod"},
            )
            self.assertEqual(
                pcl.ext_repos_missing_angled_includes(
                    tmpdir, "src/pkg", bazel_text
                ),
                ["//third_party/noinc"],
            )
            self.assertEqual(
                pcl.external_repo_test_blockers(tmpdir, "src/pkg", bazel_text),
                ["//third_party/noinc", "@rules_incompat"],
            )
            self.assertEqual(
                pcl.external_repo_test_blockers(
                    tmpdir, "src/pkg", bazel_text, device=False
                ),
                ["//third_party/noinc"],
            )
            rem = pcl.external_repo_blocked_test_remediation(
                "src/pkg", "`:t`", ["@rules_incompat"]
            )
            self.assertIn("## BAZEL2GN SENTINEL", rem)
            self.assertIn("do not list the Bazel test", rem)


import rdeps_lib as rl


class RdepsLibAndScriptsTest(unittest.TestCase):
    def test_visibility_recommendation_tiers(self):
        # Tier 1: narrow
        self.assertEqual(
            rl.recommend_narrow_visibility(
                ["src/sys/pkg/bin/pkg-cache", "src/sys/pkg/lib/fuchsia-pkg"],
                "src/lib/my_lib",
            ),
            [
                "//src/sys/pkg/bin/pkg-cache:__pkg__",
                "//src/sys/pkg/lib/fuchsia-pkg:__pkg__",
            ],
        )
        # Vendor subpackage rollup sanitization
        self.assertEqual(
            rl.recommend_narrow_visibility(
                [f"{rl.VG_ROOT}/foo/bar", "src/diagnostics/archivist"],
                "src/lib/my_lib",
            ),
            [
                "//src/diagnostics/archivist:__pkg__",
                rl.VG_SUBPKGS,
            ],
        )

        # Tier 2: area rollup when narrow > 8
        many_callers = [f"src/connectivity/sub_{i}/leaf" for i in range(5)] + [
            f"src/diagnostics/tool_{i}" for i in range(5)
        ]
        self.assertEqual(
            rl.recommend_visibility(many_callers, "src/lib/my_lib"),
            [
                "//src/connectivity:__subpackages__",
                "//src/diagnostics:__subpackages__",
            ],
        )

        # Tier 3: globally used across >= 6 areas and >= 20 callers
        global_callers = [
            f"src/{area}/comp_{i}"
            for area in (
                "connectivity",
                "diagnostics",
                "devices",
                "Modular",
                "security",
                "storage",
                "sys",
            )
            for i in range(8)
        ]
        self.assertTrue(
            rl.is_globally_used_target(global_callers, "src/lib/base")
        )

        # Multi-area test/benchmark callers -> //:__subpackages__
        test_callers = [
            "src/sys/pkg/tests/pkg-resolver",
            "src/storage/benchmarks/vfs",
        ]
        self.assertTrue(
            rl.has_only_test_or_bench_rdeps(test_callers, "src/lib/helper")
        )
        self.assertEqual(
            rl.recommend_visibility(
                test_callers, "src/lib/helper", target_name="helper"
            ),
            ["//:__subpackages__"],
        )

    def test_find_rdeps_and_visibility_audit_integration(self):
        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.abspath(os.path.join(checks_dir, "..", "tools"))
        find_rdeps_sh = os.path.join(tools_dir, "find_rdeps.sh")
        vis_audit_sh = os.path.join(checks_dir, "visibility_audit.sh")

        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.email", "test-user"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test"],
                check=True,
            )

            lib_dir = os.path.join(tmpdir, "src/lib/foo")
            caller_dir = os.path.join(tmpdir, "src/sys/bar")
            os.makedirs(lib_dir)
            os.makedirs(caller_dir)

            with open(os.path.join(lib_dir, "BUILD.gn"), "w") as f:
                f.write(
                    """
rustc_library("foo") {
  sources = [ "src/lib.rs" ]
}
group("tests") {
  testonly = true
}
"""
                )
            with open(os.path.join(caller_dir, "BUILD.gn"), "w") as f:
                f.write(
                    """
rustc_binary("bar") {
  deps = [ "//src/lib/foo:foo" ]
}
"""
                )
            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "init"], check=True
            )

            # Now migrate src/lib/foo to BUILD.bazel without visibility first
            with open(os.path.join(lib_dir, "BUILD.bazel"), "w") as f:
                f.write(
                    """
load("//build/bazel/rules/rust:defs.bzl", "rustc_library")

rustc_library(
    name = "foo",
    srcs = ["src/lib.rs"],
)
"""
                )
            env = dict(os.environ)
            env["PLANTER_WORKDIR"] = tmpdir
            env["PLANTER_TARGET_DIR"] = "src/lib/foo"
            env["PLANTER_SKIP_FETCH"] = "1"
            env["PLANTER_SKIP_DIFF_COVERAGE"] = "1"
            env.pop("PLANTER_CHANGED_FILES", None)
            env.pop("PLANTER_RESOLVED_CHANGE_BASE", None)

            rdeps_out = subprocess.check_output(
                [find_rdeps_sh, "src/lib/foo"], env=env, text=True
            )
            rdeps_data = json.loads(rdeps_out)
            self.assertEqual(
                rdeps_data["true_rdep_packages"], ["//src/sys/bar"]
            )
            self.assertEqual(
                rdeps_data["per_target_recommended_visibility"]["foo"],
                ["//src/sys/bar:__pkg__"],
            )

            # visibility_audit should report missing_target_visibility
            va_out = subprocess.check_output([vis_audit_sh], env=env, text=True)
            va_findings = json.loads(va_out)
            self.assertEqual(len(va_findings), 1)
            self.assertEqual(
                va_findings[0]["category"], "missing_target_visibility"
            )

            # Now set the recommended visibility on foo
            with open(os.path.join(lib_dir, "BUILD.bazel"), "w") as f:
                f.write(
                    """
load("//build/bazel/rules/rust:defs.bzl", "rustc_library")

rustc_library(
    name = "foo",
    srcs = ["src/lib.rs"],
    visibility = ["//src/sys/bar:__pkg__"],
)
"""
                )
            va_out_clean = subprocess.check_output(
                [vis_audit_sh], env=env, text=True
            )
            self.assertEqual(json.loads(va_out_clean), [])

    def test_manifest_and_findings_parsing(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            mpath = os.path.join(tmpdir, "manifest.json")
            with open(mpath, "w") as f:
                json.dump(
                    {
                        "version": 1,
                        "checks": [
                            {
                                "name": "vis_check",
                                "description": "Visibility check",
                                "script_file": "visibility_audit.sh",
                                "args": ["--strict"],
                                "enabled": True,
                            }
                        ],
                    },
                    f,
                )
            specs = pcl.load_check_manifest(mpath)
            self.assertEqual(len(specs), 1)
            self.assertEqual(specs[0]["name"], "vis_check")
            self.assertTrue(specs[0]["enabled"])

            # Recovery of trailing JSON array after stdout noise
            noisy_raw = 'some debug log\n[{"category": "cat1", "severity": "warn", "line": 7, "message": "m"}]'
            findings, ok = pcl.parse_check_findings(noisy_raw)
            self.assertTrue(ok)
            self.assertEqual(len(findings), 1)
            self.assertEqual(findings[0]["category"], "cat1")
            self.assertEqual(findings[0]["line"], 7)

            # The explicit review_diff waiver survives parsing (planter's gate reads it).
            findings, ok = pcl.parse_check_findings(
                '[{"category": "visibility_narrowed", "severity": "info", "message": "m", "waiver": "verified"}]'
            )
            self.assertTrue(ok)
            self.assertEqual(findings[0]["waiver"], "verified")

    def test_run_checks_integration(self):
        run_checks_sh = os.path.normpath(
            os.path.join(
                os.path.dirname(os.path.abspath(__file__)),
                "..",
                "tools",
                "run_checks.sh",
            )
        )
        list_out = subprocess.check_output([run_checks_sh, "--list"], text=True)
        self.assertIn("visibility_audit:", list_out)
        self.assertIn("migration_sanity:", list_out)

        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    tmpdir,
                    "config",
                    "user.email",
                    "test-user",
                ],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test User"],
                check=True,
            )
            pkg_dir = os.path.join(tmpdir, "src", "lib", "foo")
            os.makedirs(pkg_dir)
            with open(os.path.join(pkg_dir, "BUILD.gn"), "w") as f:
                f.write('rustc_library("foo") {}\n')
            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "init"], check=True
            )

            env = dict(os.environ)
            env["PLANTER_WORKDIR"] = tmpdir
            env["PLANTER_SKIP_FETCH"] = "1"
            env.pop("PLANTER_CHANGED_FILES", None)
            env.pop("PLANTER_RESOLVED_CHANGE_BASE", None)

            proc = subprocess.run(
                [
                    run_checks_sh,
                    "--only=visibility_audit",
                    "--json",
                    "src/lib/foo",
                ],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertEqual(json.loads(proc.stdout), [])

    def test_migration_sanity_rust_next_detection_follows_split_bzl(self):
        """fidl_library() now builds rust_next via fidl_rust_next_library.bzl."""
        script = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "migration_sanity.sh"
        )
        stub = "def fidl_library(name, enable_rust_next = False, **kwargs):\n    pass\n"
        split = (
            'load(":fidl_rust_next_library.bzl", "fidl_rust_next_library")\n'
            "def fidl_library(name, enable_rust_next = False, **kwargs):\n"
            "    if enable_rust_next:\n"
            "        fidl_rust_next_library(name = name)\n"
        )
        next_bzl = 'def _suffix(flavor):\n    return "rust_fdomain_next"\n'

        def run(files):
            with tempfile.TemporaryDirectory() as tmpdir:
                for rel, text in files.items():
                    os.makedirs(
                        os.path.dirname(os.path.join(tmpdir, rel)),
                        exist_ok=True,
                    )
                    with open(os.path.join(tmpdir, rel), "w") as f:
                        f.write(text)
                subprocess.run(["git", "init", "-q", tmpdir], check=True)
                subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
                subprocess.run(
                    [
                        "git",
                        "-C",
                        tmpdir,
                        "-c",
                        "user.name=t",
                        "-c",
                        "user.email=t@x",
                        "commit",
                        "-q",
                        "-m",
                        "init",
                    ],
                    check=True,
                )
                env = dict(os.environ)
                env.update(
                    PLANTER_WORKDIR=tmpdir,
                    PLANTER_TARGET_DIRS="src/foo",
                    PLANTER_TARGET_DIR="src/foo",
                    PLANTER_SKIP_FETCH="1",
                )
                proc = subprocess.run(
                    ["bash", script],
                    env=env,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                return {
                    f.get("category") for f in json.loads(proc.stdout or "[]")
                }

        pkg = {
            "src/foo/BUILD.gn": "",
            "src/foo/BUILD.bazel": textwrap.dedent(
                """\
                rustc_library(
                    name = "foo",
                    deps = ["//src/fidl:foo_rust_next"],
                )
                """
            ),
        }
        stub_cats = run(
            {"build/bazel/rules/fidl/fidl_library.bzl": stub, **pkg}
        )
        self.assertIn("unimplemented_fidl_rust_next_dep", stub_cats)
        split_cats = run(
            {
                "build/bazel/rules/fidl/fidl_library.bzl": split,
                "build/bazel/rules/fidl/fidl_rust_next_library.bzl": next_bzl,
                **pkg,
            }
        )
        self.assertNotIn("unimplemented_fidl_rust_next_dep", split_cats)

    def test_rdeps_gni_variable_scoping_and_excluded_lists(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.email", "test-user"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test"],
                check=True,
            )

            def write(rel, content):
                p = os.path.join(tmpdir, rel)
                os.makedirs(os.path.dirname(p), exist_ok=True)
                with open(p, "w", encoding="utf-8") as f:
                    f.write(textwrap.dedent(content))

            write(
                "src/lib/multi/BUILD.gn",
                """
                rustc_library("used") {}
                rustc_library("unused") {}
                rustc_library("member") {}
                rustc_library("excluded") {}
                rustc_library("wrapped") {}
                """,
            )
            write(
                "src/lists/defs.gni",
                """
                used_list = [ "//src/lib/multi:used" ]
                unused_list = [ "//src/lib/multi:unused" ]
                allowlist_var = [ "//src/lib/multi:member" ]
                base_excl_src = [ "//src/lib/multi:excluded" ]
                combined_list = used_list
                combined_list += base_excl_src

                template("inner_wrap") {
                  group(target_name) {
                    deps = [ "//src/lib/multi:wrapped" ]
                  }
                }
                """,
            )
            write(
                "src/lists/reexport.gni",
                """
                import("//src/lists/defs.gni")

                template("outer_wrap") {
                  inner_wrap(target_name) {}
                }
                """,
            )
            write(
                "src/consumer/dep_user/BUILD.gn",
                """
                import("//src/lists/defs.gni")

                _excluded_targets = [
                  "//src/lib/multi:excluded",
                ]

                group("all") {
                  deps = combined_list
                }
                """,
            )
            write(
                "src/consumer/allowlist_user/BUILD.gn",
                """
                import("//src/lists/defs.gni")

                _is_allowed = allowlist_var + [ target_name ] - [ target_name ] != allowlist_var
                _is_not_allowed = allowlist_var != allowlist_var + [ target_name ] - [ target_name ]
                if (allowlist_var == allowlist_var + [ target_name ] - [ target_name ]) {
                }
                group("vis_only") {
                  visibility = []
                  foreach(t, allowlist_var) {
                    visibility += [ "${t}_sub" ]
                  }
                }
                """,
            )
            write(
                "src/consumer/wrap_user/BUILD.gn",
                """
                import("//src/lists/reexport.gni")

                outer_wrap("my_target") {}
                """,
            )

            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "init"], check=True
            )

            (
                true_rdeps,
                per_target_rdeps,
                per_target_direct_rdeps,
                _,
                _,
                _,
            ) = rl.compute_package_rdeps(
                tmpdir,
                "src/lib/multi",
                ["used", "unused", "member", "excluded", "wrapped"],
                set(),
                set(),
                {},
            )
            self.assertEqual(
                set(per_target_direct_rdeps["used"].keys()),
                {"src/consumer/dep_user"},
            )
            self.assertEqual(
                per_target_rdeps["used"],
                {"src/consumer/dep_user", "src/lists"},
            )
            self.assertEqual(per_target_rdeps["unused"], set())
            self.assertEqual(per_target_rdeps["member"], set())
            self.assertEqual(per_target_rdeps["excluded"], set())
            self.assertEqual(
                set(per_target_direct_rdeps["wrapped"].keys()),
                {"src/consumer/wrap_user"},
            )
            self.assertEqual(
                per_target_rdeps["wrapped"],
                {"src/consumer/wrap_user", "src/lists"},
            )
            self.assertEqual(
                true_rdeps,
                {
                    "src/consumer/dep_user",
                    "src/consumer/wrap_user",
                    "src/lists",
                },
            )

    def test_visibility_audit_preexisting_variable_bound_visibility(self):
        vis_audit_sh = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "visibility_audit.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.email", "test-user"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test"],
                check=True,
            )

            callers = []
            for area in (
                "cobalt",
                "connectivity",
                "developer",
                "diagnostics",
                "media",
                "power",
                "security",
            ):
                for idx in range(3):
                    pkg = f"src/{area}/comp_{idx}"
                    callers.append(f"//{pkg}:__pkg__")
                    cdir = os.path.join(tmpdir, pkg)
                    os.makedirs(cdir, exist_ok=True)
                    with open(os.path.join(cdir, "BUILD.bazel"), "w") as f:
                        f.write(
                            f'rustc_library(name = "c_{idx}", deps = ["//src/lib/shared"])\n'
                        )

            vis_entries_src = ",\n    ".join(f'"{c}"' for c in callers)
            shared_dir = os.path.join(tmpdir, "src/lib/shared")
            os.makedirs(shared_dir, exist_ok=True)
            with open(os.path.join(shared_dir, "BUILD.bazel"), "w") as f:
                f.write(
                    f"""SHARED_VIS = [
    {vis_entries_src},
]

SHARED_VIS += []

rustc_library(
    name = "shared",
    srcs = ["lib.rs"],
    visibility = SHARED_VIS,
)
"""
                )
            with open(os.path.join(shared_dir, "lib.rs"), "w") as f:
                f.write("pub fn f() {}\n")

            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "init"], check=True
            )

            # Edit an unrelated attribute on the target with pre-existing variable visibility.
            with open(os.path.join(shared_dir, "BUILD.bazel"), "w") as f:
                f.write(
                    f"""SHARED_VIS = [
    {vis_entries_src},
]

SHARED_VIS += []

rustc_library(
    name = "shared",
    srcs = ["lib.rs"],
    edition = "2024",
    visibility = SHARED_VIS,
)
"""
                )
            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "update target"],
                check=True,
            )

            vis_vals, no_vis, has_default = rl.get_preexisting_vis_info(
                tmpdir, "src/lib/shared/BUILD.bazel", "HEAD~1"
            )
            self.assertEqual(vis_vals, set(callers))
            self.assertEqual(no_vis, set())
            self.assertFalse(has_default)

            env = dict(os.environ)
            env["PLANTER_WORKDIR"] = tmpdir
            env["PLANTER_TARGET_DIR"] = "src/lib/shared"
            env["PLANTER_CHANGE_BASE"] = "HEAD~1"
            env["PLANTER_SKIP_FETCH"] = "1"
            env["PLANTER_SKIP_DIFF_COVERAGE"] = "1"
            env.pop("PLANTER_CHANGED_FILES", None)
            env.pop("PLANTER_RESOLVED_CHANGE_BASE", None)

            va_out = subprocess.check_output([vis_audit_sh], env=env, text=True)
            errors = [
                f for f in json.loads(va_out) if f.get("severity") == "error"
            ]
            self.assertEqual(errors, [])

            # Also verify uncommitted edits (change_base == HEAD) resolve
            # pre-existing variable-bound visibility from HEAD while catching
            # a newly added sprawling variable-bound visibility list.
            other_callers = []
            for area in (
                "cobalt",
                "connectivity",
                "developer",
                "diagnostics",
                "media",
                "power",
                "security",
            ):
                for idx in range(3):
                    opkg = f"src/{area}/other_{idx}"
                    other_callers.append(f"//{opkg}:__pkg__")
                    odir = os.path.join(tmpdir, opkg)
                    os.makedirs(odir, exist_ok=True)
                    with open(os.path.join(odir, "BUILD.bazel"), "w") as f:
                        f.write(
                            f'rustc_library(name = "o_{idx}", deps = ["//src/lib/shared:shared_new"])\n'
                        )
            other_vis_entries_src = ",\n    ".join(
                f'"{c}"' for c in other_callers
            )
            with open(os.path.join(shared_dir, "BUILD.bazel"), "w") as f:
                f.write(
                    f"""SHARED_VIS = [
    {vis_entries_src},
]

SHARED_VIS += []

NEW_SPRAWLING_VIS = [
    {other_vis_entries_src},
]

rustc_library(
    name = "shared",
    srcs = ["lib.rs"],
    edition = "2024",
    visibility = SHARED_VIS,
)

rustc_library(
    name = "shared_new",
    srcs = ["lib.rs"],
    visibility = NEW_SPRAWLING_VIS,
)
"""
                )
            env["PLANTER_CHANGE_BASE"] = "HEAD"
            va_wt_out = subprocess.check_output(
                [vis_audit_sh], env=env, text=True
            )
            wt_errors = [
                f for f in json.loads(va_wt_out) if f.get("severity") == "error"
            ]
            self.assertEqual(len(wt_errors), 1)
            self.assertEqual(
                wt_errors[0]["category"], "overly_narrow_global_visibility"
            )
            self.assertIn("shared_new", wt_errors[0]["message"])

    def test_two_tier_test_suite_check_fixes(self):
        checks_dir = os.path.dirname(os.path.abspath(__file__))
        tools_dir = os.path.abspath(os.path.join(checks_dir, "..", "tools"))
        ann_sh = os.path.join(checks_dir, "cc_header_deps_and_annotations.sh")
        vis_audit_sh = os.path.join(checks_dir, "visibility_audit.sh")
        cq_reach_sh = os.path.join(checks_dir, "cq_reachability.sh")
        run_checks_sh = os.path.join(tools_dir, "run_checks.sh")

        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.email", "test-user"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test"],
                check=True,
            )

            def write(rel, content):
                p = os.path.join(tmpdir, rel)
                os.makedirs(os.path.dirname(p), exist_ok=True)
                with open(p, "w", encoding="utf-8") as f:
                    f.write(textwrap.dedent(content))

            # Set up origin/main commit and ancestor aggregation structure.
            write(
                "build/bazel/rules/testing/fx_test.bzl",
                "def fx_test(**kw): pass\n",
            )
            write(
                "src/area/BUILD.bazel",
                """
                # @bazel2gn:skip
                test_suite(
                    name = "area_tests",
                    tests = ["//src/area/sub:sub_tests"],
                )
                """,
            )
            write(
                "src/area/BUILD.gn",
                """
                group("tests") {
                  testonly = true
                }
                """,
            )
            write(
                "src/area/sub/pkg/BUILD.gn",
                """
                ## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##
                rustc_library("pkg") {}
                """,
            )
            write(
                "src/dual_anc/BUILD.gn",
                """
                bazel_test_suite("bazel_tests") {
                  target_tests = [ "//src/dual_anc:target_tests" ]
                }
                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                ## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##
                rustc_library("dual_anc") {}
                """,
            )
            write(
                "src/dual_anc/child/BUILD.gn",
                """
                group("tests") {
                  testonly = true
                  deps = [ ":child_test($host_toolchain)" ]
                }
                rustc_library("child") {}
                rustc_test("child_test") {}
                """,
            )
            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "init"], check=True
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    tmpdir,
                    "update-ref",
                    "refs/remotes/origin/main",
                    "HEAD",
                ],
                check=True,
            )

            # 1. # @bazel2gn:skip above test_suite() does not require a comment,
            # while an undocumented # @bazel2gn:skip on rustc_library does.
            write(
                "src/area/sub/pkg/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_library", "rustc_test")

                # @bazel2gn:skip
                test_suite(
                    name = "tests",
                    tests = [":pkg_test"],
                    visibility = ["//src/area:__pkg__"],
                )

                # @bazel2gn:skip
                rustc_library(
                    name = "pkg",
                    srcs = ["lib.rs"],
                )

                rustc_test(
                    name = "pkg_test",
                    deps = [":pkg"],
                )
                """,
            )
            env = dict(os.environ)
            env["PLANTER_WORKDIR"] = tmpdir
            env["PLANTER_TARGET_DIR"] = "src/area/sub/pkg"
            env["PLANTER_TARGET_DIRS"] = "src/area/sub/pkg"
            env["PLANTER_CHANGE_BASE"] = "HEAD"
            env["PLANTER_SKIP_FETCH"] = "1"
            env["PLANTER_SKIP_DIFF_COVERAGE"] = "1"
            env.pop("PLANTER_CHANGED_FILES", None)
            env.pop("PLANTER_RESOLVED_CHANGE_BASE", None)

            ann_findings = json.loads(
                subprocess.check_output([ann_sh], env=env, text=True)
            )
            undoc_lines = [
                f["line"]
                for f in ann_findings
                if f.get("category") == "undocumented_bazel2gn_directive"
            ]
            self.assertEqual(undoc_lines, [11])

            # 2. visibility_audit allows ancestor visibility on exposed package
            # test_suite("tests") and intermediate aggregation test_suite("sub_tests"),
            # but flags individual tests and private local sub-suites.
            write(
                "src/area/sub/pkg/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                # @bazel2gn:skip
                test_suite(
                    name = "tests",
                    tests = [":pkg_test"],
                    visibility = ["//src/area:__pkg__"],
                )

                # @bazel2gn:skip
                test_suite(
                    name = "sub_tests",
                    tests = ["//src/area/sub/other:tests"],
                    visibility = ["//src/area:__pkg__"],
                )

                # @bazel2gn:skip
                test_suite(
                    name = "local_helper_suite",
                    tests = [":pkg_test"],
                    visibility = ["//src/area:__pkg__"],
                )

                rustc_test(
                    name = "pkg_test",
                    visibility = ["//src/area:__pkg__"],
                )
                """,
            )
            va_findings = json.loads(
                subprocess.check_output([vis_audit_sh], env=env, text=True)
            )
            fp_msgs = [
                f["message"]
                for f in va_findings
                if f.get("category") == "false_positive_rdep_visibility"
            ]
            self.assertEqual(len(fp_msgs), 2)
            self.assertTrue(any("local_helper_suite" in m for m in fp_msgs))
            self.assertTrue(any("pkg_test" in m for m in fp_msgs))

            # 3. cq_reachability's test_suite remediation never asks for
            # `# @bazel2gn:skip` (bazel2gn ignores test_suite), even when the
            # ancestor BUILD.gn has a BAZEL2GN SENTINEL.
            write(
                "src/dual_anc/child/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "host_rustc_test", "rustc_library")

                rustc_library(
                    name = "child",
                    srcs = ["lib.rs"],
                )

                host_rustc_test(
                    name = "child_test",
                    deps = [":child"],
                )
                """,
            )
            env_dual = dict(env)
            env_dual["PLANTER_TARGET_DIR"] = "src/dual_anc/child"
            env_dual["PLANTER_TARGET_DIRS"] = "src/dual_anc/child"
            cq_findings = json.loads(
                subprocess.check_output([cq_reach_sh], env=env_dual, text=True)
            )
            unexp = [
                f
                for f in cq_findings
                if f.get("category") == "unexported_bazel_test"
            ]
            self.assertEqual(len(unexp), 1)
            self.assertIn(
                '`test_suite(name = "host_tests"',
                unexp[0]["remediation"],
            )
            self.assertNotIn("@bazel2gn:skip", unexp[0]["remediation"])

            # 4. run_checks._is_missing_agg_disputable_finding matches child
            # subpackages and overflow summary findings.
            with open(run_checks_sh, "r", encoding="utf-8") as rcf:
                rc_src = rcf.read()
            m_fn = re.search(
                r"(def _missing_agg_disputable_dirs.*?\n(?=def _format_script_blocking_summary))",
                rc_src,
                re.S,
            )
            self.assertIsNotNone(m_fn)
            ns = {"os": os}
            exec(m_fn.group(1), ns)
            is_disp = ns["_is_missing_agg_disputable_finding"]
            sample_findings = [
                {
                    "source": "cq_reachability",
                    "category": "unexported_bazel_test",
                    "file": "src/foo/BUILD.bazel",
                    "remediation": "report in `CoderReport.disputed_findings`",
                },
                {
                    "source": "tests_json_parity",
                    "category": "test_dropped_from_tests_json",
                    "file": "src/foo/sub/BUILD.gn",
                },
                {
                    "source": "tests_json_parity",
                    "category": "test_dropped_from_tests_json",
                    "file": "",
                },
            ]
            self.assertTrue(is_disp(sample_findings[1], sample_findings))
            self.assertTrue(is_disp(sample_findings[2], sample_findings))
            sample_with_unrelated = sample_findings + [
                {
                    "source": "tests_json_parity",
                    "category": "test_dropped_from_tests_json",
                    "file": "src/other/BUILD.gn",
                }
            ]
            self.assertFalse(is_disp(sample_findings[2], sample_with_unrelated))

            # 5. cq_reachability prefers keeping "tests" and "host_tests" in
            # the same aggregation BUILD.bazel package and flags split
            # aggregation across different BUILD.bazel files.
            write(
                "src/BUILD.bazel",
                """
                test_suite(
                    name = "host_tests",
                    tests = [],
                )
                """,
            )
            write(
                "src/BUILD.gn",
                """
                bazel_test_suite("bazel_tests") {
                  host_tests = [ "//src:host_tests" ]
                }
                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            # Even though src/BUILD.bazel defines host_tests at depth 0,
            # src/dual_anc (depth 1) already defines target_tests, so
            # cq_reachability recommends adding companion host_tests to
            # src/dual_anc/BUILD.bazel instead of splitting into src/BUILD.bazel.
            cq_deeper = json.loads(
                subprocess.check_output([cq_reach_sh], env=env_dual, text=True)
            )
            unexp_deeper = [
                f
                for f in cq_deeper
                if f.get("category") == "unexported_bazel_test"
            ]
            self.assertEqual(len(unexp_deeper), 1)
            self.assertIn(
                "src/dual_anc/BUILD.bazel", unexp_deeper[0]["remediation"]
            )
            self.assertNotIn("src/BUILD.bazel", unexp_deeper[0]["remediation"])

            # Umbrella aggregation package (e.g. sdk/BUILD.bazel) that already
            # references //sdk/rust/iob:host_tests should also receive
            # companion //sdk/rust/iob:tests instead of being disputed.
            write(
                "sdk/BUILD.bazel",
                """
                test_suite(
                    name = "host_tests",
                    tests = [
                        "//sdk/rust/iob:host_tests",
                    ],
                )
                """,
            )
            write(
                "sdk/BUILD.gn",
                """
                bazel_test_suite("bazel_tests") {
                  host_tests = [ "//sdk:host_tests" ]
                }
                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            write(
                "sdk/rust/iob/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "host_rustc_test", "rustc_library")
                load("//build/bazel/rules/testing:fx_test.bzl", "fx_unittest_package")

                test_suite(
                    name = "tests",
                    tests = [":iob_device_test"],
                )

                test_suite(
                    name = "host_tests",
                    target_compatible_with = HOST_OS_CONSTRAINTS,
                    tests = [":iob_host_test"],
                    visibility = ["//sdk:__pkg__"],
                )

                rustc_library(
                    name = "iob",
                    srcs = ["lib.rs"],
                )

                fx_unittest_package(
                    name = "iob_device_test",
                )

                host_rustc_test(
                    name = "iob_host_test",
                    deps = [":iob"],
                )
                """,
            )
            env_sdk = dict(env)
            env_sdk["PLANTER_TARGET_DIR"] = "sdk/rust/iob"
            env_sdk["PLANTER_TARGET_DIRS"] = "sdk/rust/iob"
            cq_sdk = json.loads(
                subprocess.check_output([cq_reach_sh], env=env_sdk, text=True)
            )
            unexp_sdk = [
                f
                for f in cq_sdk
                if f.get("category") == "unexported_bazel_test"
            ]
            self.assertEqual(len(unexp_sdk), 1)
            self.assertIn("sdk/BUILD.bazel", unexp_sdk[0]["remediation"])
            self.assertIn(
                'test_suite(name = "tests"', unexp_sdk[0]["remediation"]
            )
            self.assertNotIn(
                'test_suite(name = "target_tests"', unexp_sdk[0]["remediation"]
            )
            self.assertNotIn("disputed_findings", unexp_sdk[0]["remediation"])

            # Splitting //sdk/rust/iob:host_tests (in sdk/BUILD.bazel) and
            # //sdk/rust/iob:tests (in sdk/rust/BUILD.bazel) emits an error.
            write(
                "sdk/rust/BUILD.bazel",
                """
                test_suite(
                    name = "tests",
                    tests = [
                        "//sdk/rust/iob:tests",
                    ],
                )
                """,
            )
            write(
                "sdk/rust/BUILD.gn",
                """
                bazel_test_suite("bazel_tests") {
                  target_tests = [ "//sdk/rust:tests" ]
                }
                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            cq_split = json.loads(
                subprocess.check_output([cq_reach_sh], env=env_sdk, text=True)
            )
            split_findings = [
                f
                for f in cq_split
                if f.get("category") == "split_package_test_suite_aggregation"
            ]
            self.assertEqual(len(split_findings), 1)
            self.assertEqual(split_findings[0]["severity"], "error")
            self.assertEqual(
                split_findings[0]["file"], "sdk/rust/iob/BUILD.bazel"
            )
            self.assertIn("sdk/BUILD.bazel", split_findings[0]["remediation"])
            self.assertIn(
                "sdk/rust/BUILD.bazel", split_findings[0]["remediation"]
            )

            # Equal-depth sibling aggregation packages (sdk/rust vs sdk/longer_sibling)
            # use the equal-depth recommendation branch rather than preferring the
            # longer directory string.
            write(
                "sdk/BUILD.bazel",
                """
                test_suite(
                    name = "host_tests",
                    tests = [],
                )
                """,
            )
            write(
                "sdk/longer_sibling/BUILD.bazel",
                """
                test_suite(
                    name = "host_tests",
                    tests = [
                        "//sdk/rust/iob:host_tests",
                    ],
                )
                """,
            )
            write(
                "sdk/longer_sibling/BUILD.gn",
                """
                bazel_test_suite("bazel_tests") {
                  host_tests = [ "//sdk/longer_sibling:host_tests" ]
                }
                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            cq_sibling_split = json.loads(
                subprocess.check_output([cq_reach_sh], env=env_sdk, text=True)
            )
            sibling_split_findings = [
                f
                for f in cq_sibling_split
                if f.get("category") == "split_package_test_suite_aggregation"
            ]
            self.assertEqual(len(sibling_split_findings), 1)
            self.assertIn(
                "either 'sdk/rust/BUILD.bazel' or 'sdk/longer_sibling/BUILD.bazel'",
                sibling_split_findings[0]["remediation"],
            )
            write(
                "sdk/longer_sibling/BUILD.bazel",
                """
                test_suite(
                    name = "host_tests",
                    tests = [],
                )
                """,
            )

            # Consolidating both into sdk/BUILD.bazel clears the finding.
            write(
                "sdk/rust/BUILD.bazel",
                """
                test_suite(
                    name = "tests",
                    tests = [],
                )
                """,
            )
            write(
                "sdk/BUILD.bazel",
                """
                test_suite(
                    name = "tests",
                    tests = [
                        "//sdk/rust/iob:tests",
                    ],
                )

                test_suite(
                    name = "host_tests",
                    tests = [
                        "//sdk/rust/iob:host_tests",
                    ],
                )
                """,
            )
            write(
                "sdk/BUILD.gn",
                """
                bazel_test_suite("bazel_tests") {
                  target_tests = [ "//sdk:tests" ]
                  host_tests = [ "//sdk:host_tests" ]
                }
                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            cq_consolidated = json.loads(
                subprocess.check_output([cq_reach_sh], env=env_sdk, text=True)
            )
            self.assertEqual(
                [
                    f
                    for f in cq_consolidated
                    if f.get("category")
                    in (
                        "split_package_test_suite_aggregation",
                        "unexported_bazel_test",
                    )
                ],
                [],
            )

    def test_test_suite_layout_sibling_aggregation(self):
        layout_sh = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "test_suite_layout.sh"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.email", "test-user"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test"],
                check=True,
            )

            def write(rel, content):
                p = os.path.join(tmpdir, rel)
                os.makedirs(os.path.dirname(p), exist_ok=True)
                with open(p, "w", encoding="utf-8") as f:
                    f.write(textwrap.dedent(content))

            write(
                "src/foo/BUILD.gn",
                """
                group("tests") {
                  testonly = true
                  deps = [ "tests" ]
                }
                """,
            )
            write(
                "src/foo/bar/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [":bar_test"],
                    visibility = ["//src/foo/tests:__pkg__"],
                )

                rustc_test(
                    name = "bar_test",
                )
                """,
            )
            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "init"], check=True
            )

            # Dedicated sibling-aggregating package in src/foo/tests is valid.
            write(
                "src/foo/tests/BUILD.gn",
                """
                import("//build/bazel/bazel_test_suite.gni")

                bazel_test_suite("bazel_tests") {
                  target_tests = [ "//src/foo/tests:tests" ]
                }

                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            write(
                "src/foo/tests/BUILD.bazel",
                """
                test_suite(
                    name = "tests",
                    tests = ["//src/foo/bar:tests"],
                )
                """,
            )
            env = dict(os.environ)
            env["PLANTER_WORKDIR"] = tmpdir
            env["PLANTER_TARGET_DIR"] = "src/foo/tests"
            env["PLANTER_TARGET_DIRS"] = "src/foo/tests"
            env["PLANTER_CHANGE_BASE"] = "HEAD"
            env.pop("PLANTER_CHANGED_FILES", None)
            env.pop("PLANTER_RESOLVED_CHANGE_BASE", None)

            out = json.loads(
                subprocess.check_output([layout_sh], env=env, text=True)
            )
            self.assertEqual(out, [])

            # Leaf suite-only BUILD.gn (no cross-package aggregation) is rejected.
            write(
                "src/foo/leaf/BUILD.gn",
                """
                import("//build/bazel/bazel_test_suite.gni")

                bazel_test_suite("bazel_tests") {
                  target_tests = [ "//src/foo/leaf:tests" ]
                }

                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            write(
                "src/foo/leaf/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [":leaf_test"],
                )

                rustc_test(
                    name = "leaf_test",
                )
                """,
            )
            env_leaf = dict(env)
            env_leaf["PLANTER_TARGET_DIR"] = "src/foo/leaf"
            env_leaf["PLANTER_TARGET_DIRS"] = "src/foo/leaf"
            leaf_out = json.loads(
                subprocess.check_output([layout_sh], env=env_leaf, text=True)
            )
            leaf_cats = {f.get("category") for f in leaf_out}
            self.assertIn("per_package_bazel_test_suite", leaf_cats)
            self.assertIn("bazel_test_suite_only_build_gn", leaf_cats)

    def test_explicit_test_suite_targets_and_expanded_empty_suite(self):
        checks_dir = os.path.dirname(os.path.abspath(__file__))
        layout_sh = os.path.join(checks_dir, "test_suite_layout.sh")
        cq_reach_sh = os.path.join(checks_dir, "cq_reachability.sh")

        with tempfile.TemporaryDirectory() as tmpdir:
            subprocess.run(["git", "-C", tmpdir, "init", "-q"], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.email", "test-user"],
                check=True,
            )
            subprocess.run(
                ["git", "-C", tmpdir, "config", "user.name", "Test"],
                check=True,
            )

            def write(rel, content):
                p = os.path.join(tmpdir, rel)
                os.makedirs(os.path.dirname(p), exist_ok=True)
                with open(p, "w", encoding="utf-8") as f:
                    f.write(textwrap.dedent(content))

            write(
                "build/bazel/rules/testing/fx_test.bzl",
                "def fx_test(**kw): pass\n",
            )
            write(
                "src/area/BUILD.bazel",
                """
                test_suite(
                    name = "area_tests",
                    tests = ["//src/area/pkg:tests"],
                )
                """,
            )
            write(
                "src/area/BUILD.gn",
                """
                bazel_test_suite("bazel_tests") {
                  target_tests = [ "//src/area:area_tests" ]
                }
                group("tests") {
                  testonly = true
                  deps = [ ":bazel_tests" ]
                }
                """,
            )
            write(
                "src/area/pkg/BUILD.gn",
                """
                group("tests") {
                  testonly = true
                  deps = [ ":foo_test($host_toolchain)" ]
                }
                rustc_test("foo_test") {}
                """,
            )
            write(
                "src/area/preexisting/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [],
                )

                rustc_test(
                    name = "existing_test",
                )

                rustc_test(
                    name = "second_test",
                )
                """,
            )
            subprocess.run(["git", "-C", tmpdir, "add", "."], check=True)
            subprocess.run(
                ["git", "-C", tmpdir, "commit", "-q", "-m", "init"], check=True
            )
            subprocess.run(
                [
                    "git",
                    "-C",
                    tmpdir,
                    "update-ref",
                    "refs/remotes/origin/main",
                    "HEAD",
                ],
                check=True,
            )

            env = dict(os.environ)
            env["PLANTER_WORKDIR"] = tmpdir
            env["PLANTER_TARGET_DIR"] = "src/area/pkg"
            env["PLANTER_TARGET_DIRS"] = "src/area/pkg"
            env["PLANTER_CHANGE_BASE"] = "HEAD"
            env["PLANTER_SKIP_FETCH"] = "1"
            env["PLANTER_SKIP_DIFF_COVERAGE"] = "1"
            env.pop("PLANTER_CHANGED_FILES", None)
            env.pop("PLANTER_RESOLVED_CHANGE_BASE", None)

            def run_layout_check(target_dir="src/area/pkg"):
                local_env = dict(env)
                local_env["PLANTER_TARGET_DIR"] = target_dir
                local_env["PLANTER_TARGET_DIRS"] = target_dir
                out = subprocess.check_output(
                    [layout_sh], env=local_env, text=True
                )
                return json.loads(out)

            # 1. tests = [] (even with a comment) triggers implicit_test_suite ERROR.
            write(
                "src/area/pkg/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [],  # Include all tests in the package.
                    visibility = ["//src/area:__pkg__"],
                )

                rustc_test(
                    name = "foo_test",
                )
                """,
            )
            f1 = [
                f
                for f in run_layout_check()
                if f.get("category") == "implicit_test_suite"
            ]
            self.assertEqual(len(f1), 1)
            self.assertEqual(f1[0]["severity"], "error")
            self.assertIn('tests = [":foo_test"]', f1[0]["remediation"])

            # 2. Omitting tests triggers implicit_test_suite ERROR.
            write(
                "src/area/pkg/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    visibility = ["//src/area:__pkg__"],
                )

                rustc_test(
                    name = "foo_test",
                )
                """,
            )
            f2 = [
                f
                for f in run_layout_check()
                if f.get("category") == "implicit_test_suite"
            ]
            self.assertEqual(len(f2), 1)
            self.assertEqual(f2[0]["severity"], "error")
            self.assertIn('tests = [":foo_test"]', f2[0]["remediation"])

            # 3. Explicit tests = [":foo_test"] on a new test_suite passes cleanly.
            write(
                "src/area/pkg/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [":foo_test"],
                    visibility = ["//src/area:__pkg__"],
                )

                rustc_test(
                    name = "foo_test",
                )
                """,
            )
            self.assertEqual(run_layout_check(), [])

            # 4. Untouched pre-existing test_suite(tests = []) passes when only
            # a non-test target is added/modified and no tests change.
            write(
                "src/area/preexisting/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_library", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [],
                )

                rustc_library(
                    name = "preexisting",
                    srcs = ["lib.rs"],
                )

                rustc_test(
                    name = "existing_test",
                )

                rustc_test(
                    name = "second_test",
                )
                """,
            )
            self.assertEqual(run_layout_check("src/area/preexisting"), [])

            # 5. Adding a new test to a package with an existing empty test_suite:
            # (a) leaving tests = [] fails with implicit_test_suite.
            write(
                "src/area/preexisting/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [],
                )

                rustc_test(
                    name = "existing_test",
                )

                rustc_test(
                    name = "second_test",
                )

                rustc_test(
                    name = "new_test",
                )
                """,
            )
            f5a = run_layout_check("src/area/preexisting")
            self.assertEqual(
                [f["category"] for f in f5a], ["implicit_test_suite"]
            )
            self.assertIn(
                'tests = [":existing_test", ":new_test", ":second_test"]',
                f5a[0]["remediation"],
            )

            # (b) listing only `:new_test` while omitting `:existing_test` and `:second_test`
            # fails with implicit_test_suite naming the missing existing tests.
            write(
                "src/area/preexisting/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [":new_test"],
                )

                rustc_test(
                    name = "existing_test",
                )

                rustc_test(
                    name = "second_test",
                )

                rustc_test(
                    name = "new_test",
                )
                """,
            )
            f5b = run_layout_check("src/area/preexisting")
            self.assertEqual(
                [f["category"] for f in f5b], ["implicit_test_suite"]
            )
            self.assertIn(":existing_test", f5b[0]["message"])
            self.assertIn(":second_test", f5b[0]["message"])

            # (c) including all existing tests plus `:new_test` emits expanded_empty_test_suite
            # instructing the coder to raise a disputed_findings comment for a human reviewer
            # that agents must not resolve.
            write(
                "src/area/preexisting/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [
                        ":existing_test",
                        ":new_test",
                        ":second_test",
                    ],
                )

                rustc_test(
                    name = "existing_test",
                )

                rustc_test(
                    name = "second_test",
                )

                rustc_test(
                    name = "new_test",
                )
                """,
            )
            f5c = run_layout_check("src/area/preexisting")
            self.assertEqual(
                [f["category"] for f in f5c], ["expanded_empty_test_suite"]
            )
            self.assertIn(
                "CoderReport.disputed_findings", f5c[0]["remediation"]
            )
            self.assertIn("Agents must NEVER resolve", f5c[0]["remediation"])

            # 6. Removing a test from a package with an existing empty test_suite:
            # (a) keeping the removed test in tests = [...] fails with implicit_test_suite.
            write(
                "src/area/preexisting/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [
                        ":existing_test",
                        ":second_test",
                    ],
                )

                rustc_test(
                    name = "existing_test",
                )
                """,
            )
            f6a = run_layout_check("src/area/preexisting")
            self.assertEqual(
                [f["category"] for f in f6a], ["implicit_test_suite"]
            )
            self.assertIn(":second_test", f6a[0]["message"])

            # (b) starting from the existing tests and removing `:second_test` emits
            # expanded_empty_test_suite.
            write(
                "src/area/preexisting/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "rustc_test")

                test_suite(
                    name = "tests",
                    tests = [":existing_test"],
                )

                rustc_test(
                    name = "existing_test",
                )
                """,
            )
            f6b = run_layout_check("src/area/preexisting")
            self.assertEqual(
                [f["category"] for f in f6b], ["expanded_empty_test_suite"]
            )

            # 7. cq_reachability remediation specifies explicit tests = [":foo_test"].
            write(
                "src/area/pkg/BUILD.bazel",
                """
                load("//build/bazel/rules/rust:defs.bzl", "host_rustc_test")

                host_rustc_test(
                    name = "foo_test",
                    srcs = ["lib.rs"],
                )
                """,
            )
            cq_findings = json.loads(
                subprocess.check_output([cq_reach_sh], env=env, text=True)
            )
            unexp = [
                f
                for f in cq_findings
                if f.get("category") == "unexported_bazel_test"
            ]
            self.assertEqual(len(unexp), 1)
            self.assertIn('tests = [":foo_test"]', unexp[0]["remediation"])
            self.assertNotIn("tests = []", unexp[0]["remediation"])

            # 8. Test binary wrapped by fuchsia_test_component + fuchsia_test_package
            # is recognized as a packaged helper and does not require listing the
            # private binary in test_suite(name = "tests").
            write(
                "src/area/pkg/BUILD.bazel",
                """
                load("@rules_fuchsia//fuchsia:defs.bzl", "fuchsia_test_component", "fuchsia_test_package")
                load("//build/bazel/rules/cc:defs.bzl", "cc_test")

                test_suite(
                    name = "tests",
                    tests = [":pkg_test"],
                    visibility = ["//src/area:__pkg__"],
                )

                cc_test(
                    name = "pkg_test_bin",
                    srcs = ["test.cc"],
                )

                fuchsia_test_component(
                    name = "pkg_test_cmp",
                    deps = [":pkg_test_bin"],
                )

                fuchsia_test_package(
                    name = "pkg_test",
                    test_components = [":pkg_test_cmp"],
                )
                """,
            )
            self.assertEqual(
                [
                    f
                    for f in run_layout_check("src/area/pkg")
                    if f["file"] == "src/area/pkg/BUILD.bazel"
                ],
                [],
            )


if __name__ == "__main__":
    unittest.main()
