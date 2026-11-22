#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Tests for empty_build_bazel and includecode checks in migration_sanity.sh."""

import json
import os
import subprocess
import tempfile
import unittest

SCRIPT = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "migration_sanity.sh"
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


def _write_file(repo: str, rel_path: str, content: str) -> None:
    full_path = os.path.join(repo, rel_path)
    os.makedirs(os.path.dirname(full_path), exist_ok=True)
    with open(full_path, "w", encoding="utf-8") as f:
        f.write(content)


def _run_migration_sanity(
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


class MigrationSanityIncludecodeAndEmptyBazelTest(unittest.TestCase):
    def test_empty_build_bazel_flagged_when_no_targets_or_bzl_files(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/foo/BUILD.gn": HEADER
                    + 'source_set("foo") {\n  sources = [ "foo.cc" ]\n}\n',
                },
            )
            bz_rel = "src/foo/BUILD.bazel"
            _write_file(
                tmpdir,
                bz_rel,
                HEADER
                + 'package(default_applicable_licenses = ["//:license"])\n',
            )
            findings = _run_migration_sanity(tmpdir, base, "src/foo", [bz_rel])
            cats = [f["category"] for f in findings]
            self.assertIn("empty_build_bazel", cats, findings)
            empty_f = next(
                f for f in findings if f["category"] == "empty_build_bazel"
            )
            self.assertEqual(empty_f["severity"], "error")
            self.assertEqual(empty_f["file"], bz_rel)

    def test_bzl_package_boundary_and_exports_files_not_flagged(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"README.md": "init\n"})
            bzl_pkg_bz = "tools/rules/BUILD.bazel"
            _write_file(tmpdir, "tools/rules/helper.bzl", 'HELPER = "ok"\n')
            _write_file(
                tmpdir,
                bzl_pkg_bz,
                HEADER
                + 'package(default_applicable_licenses = ["//:license"])\n',
            )
            exp_bz = "src/data/BUILD.bazel"
            _write_file(
                tmpdir,
                exp_bz,
                HEADER
                + 'package(default_applicable_licenses = ["//:license"])\n\n'
                + 'exports_files(["config.json"])\n',
            )
            findings = _run_migration_sanity(
                tmpdir, base, "src/data", [bzl_pkg_bz, exp_bz]
            )
            cats = [f["category"] for f in findings]
            self.assertNotIn("empty_build_bazel", cats, findings)

    def test_includecode_region_tag_removed_flagged(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            gn_rel = "examples/echo/BUILD.gn"
            doc_rel = "docs/development/echo.md"
            base = _init_repo(
                tmpdir,
                {
                    gn_rel: HEADER
                    + "# [START bin]\n"
                    + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n'
                    + "# [END bin]\n"
                    + "# [START unused]\n"
                    + 'group("extra") {}\n'
                    + "# [END unused]\n",
                    doc_rel: "# Echo\n\n"
                    + "{% includecode\n"
                    + '   gerrit_path="examples/echo/BUILD.gn"\n'
                    + '   region_tag="bin" %}\n',
                },
            )
            bz_rel = "examples/echo/BUILD.bazel"
            _write_file(
                tmpdir,
                bz_rel,
                HEADER
                + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_binary")\n\n'
                + 'package(default_applicable_licenses = ["//:license"])\n\n'
                + 'fx_cc_binary(\n    name = "bin",\n    srcs = ["main.cc"],\n)\n',
            )
            _write_file(
                tmpdir,
                gn_rel,
                HEADER
                + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n',
            )
            findings = _run_migration_sanity(
                tmpdir, base, "examples/echo", [gn_rel, bz_rel]
            )
            inc_findings = [
                f
                for f in findings
                if f["category"] == "includecode_region_tag_removed"
            ]
            self.assertEqual(len(inc_findings), 1, findings)
            self.assertEqual(inc_findings[0]["severity"], "error")
            self.assertIn("bin", inc_findings[0]["message"])
            self.assertIn(doc_rel, inc_findings[0]["message"])
            self.assertNotIn("unused", inc_findings[0]["message"])

    def test_includecode_preserved_above_sentinel_passes(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            gn_rel = "examples/echo/BUILD.gn"
            doc_rel = "docs/development/echo.md"
            base = _init_repo(
                tmpdir,
                {
                    gn_rel: HEADER
                    + "#[START pkg]\n"
                    + 'fuchsia_package("echo_pkg") {\n  deps = [ ":lib" ]\n}\n'
                    + "#[END pkg]\n"
                    + 'source_set("lib") {\n  sources = [ "lib.cc" ]\n}\n',
                    doc_rel: "# Echo\n\n"
                    + '{% includecode gerrit_path="examples/echo/BUILD.gn" region_tag="pkg" %}\n',
                },
            )
            bz_rel = "examples/echo/BUILD.bazel"
            _write_file(
                tmpdir,
                bz_rel,
                HEADER
                + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                + 'package(default_applicable_licenses = ["//:license"])\n\n'
                + 'fx_cc_library(\n    name = "lib",\n    srcs = ["lib.cc"],\n)\n',
            )
            _write_file(
                tmpdir,
                gn_rel,
                HEADER
                + "#[START pkg]\n"
                + 'fuchsia_package("echo_pkg") {\n  deps = [ ":lib" ]\n}\n'
                + "#[END pkg]\n\n"
                + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
                + 'source_set("lib") {\n  sources = [ "lib.cc" ]\n}\n',
            )
            findings = _run_migration_sanity(
                tmpdir, base, "examples/echo", [gn_rel, bz_rel]
            )
            cats = [f["category"] for f in findings]
            self.assertNotIn("includecode_region_tag_removed", cats, findings)

    def test_deleted_build_gn_with_whole_file_includecode_flagged(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            gn_rel = "examples/tool/BUILD.gn"
            doc_rel = "docs/development/tool.md"
            base = _init_repo(
                tmpdir,
                {
                    gn_rel: HEADER
                    + 'rustc_binary("tool") {\n  sources = [ "src/main.rs" ]\n}\n',
                    doc_rel: '{% includecode gerrit_path="examples/tool/BUILD.gn" %}\n',
                },
            )
            os.remove(os.path.join(tmpdir, gn_rel))
            bz_rel = "examples/tool/BUILD.bazel"
            _write_file(
                tmpdir,
                bz_rel,
                HEADER
                + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_binary")\n\n'
                + 'package(default_applicable_licenses = ["//:license"])\n\n'
                + 'rustc_binary(\n    name = "tool",\n    srcs = ["src/main.rs"],\n    edition = "2024",\n)\n',
            )
            findings = _run_migration_sanity(
                tmpdir, base, "examples/tool", [gn_rel, bz_rel]
            )
            inc_findings = [
                f
                for f in findings
                if f["category"] == "includecode_region_tag_removed"
            ]
            self.assertEqual(len(inc_findings), 1, findings)
            self.assertIn("whole file", inc_findings[0]["message"])

    def test_unmigrated_target_dir_with_docs_includecode_emits_blocked_info(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmpdir:
            gn_blocked = "examples/fidl/client_sync/BUILD.gn"
            gn_unblocked = "examples/fidl/plain/BUILD.gn"
            doc_rel = "docs/tutorials/sync-client.md"
            base = _init_repo(
                tmpdir,
                {
                    gn_blocked: HEADER
                    + "# [START bin]\n"
                    + 'executable("bin") {\n  sources = [ "main.cc" ]\n}\n'
                    + "# [END bin]\n",
                    gn_unblocked: HEADER
                    + 'executable("plain") {\n  sources = [ "main.cc" ]\n}\n',
                    doc_rel: '{% includecode gerrit_path="examples/fidl/client_sync/BUILD.gn" region_tag="bin" %}\n',
                },
            )
            blocked_findings = _run_migration_sanity(
                tmpdir, base, "examples/fidl/client_sync", []
            )
            blocked_cats = [
                (f["category"], f["severity"]) for f in blocked_findings
            ]
            self.assertIn(
                ("target_blocked_by_unmigrated_deps", "INFO"),
                blocked_cats,
                blocked_findings,
            )
            self.assertNotIn(
                ("missing_target_bazel_file", "error"),
                blocked_cats,
                blocked_findings,
            )

            plain_findings = _run_migration_sanity(
                tmpdir, base, "examples/fidl/plain", []
            )
            plain_cats = [
                (f["category"], f["severity"]) for f in plain_findings
            ]
            self.assertIn(
                ("missing_target_bazel_file", "error"),
                plain_cats,
                plain_findings,
            )

    def test_untracked_binary_doc_and_unindexed_tag_on_deleted_build_gn(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            gn_rel = "examples/custom/BUILD.gn"
            doc_rel = "docs/development/custom.md"
            base = _init_repo(
                tmpdir,
                {
                    gn_rel: HEADER
                    + 'rustc_binary("custom") {\n  sources = [ "src/main.rs" ]\n}\n',
                    doc_rel: '{% includecode gerrit_path="examples/custom/BUILD.gn" region_tag="unindexed_tag" %}\n',
                },
            )
            png_path = os.path.join(tmpdir, "docs/images/diagram.png")
            os.makedirs(os.path.dirname(png_path), exist_ok=True)
            with open(png_path, "wb") as f:
                f.write(b"\x89PNG\r\n\x1a\n\xff\xfe\x00\x80")

            blocked_findings = _run_migration_sanity(
                tmpdir, base, "examples/custom", []
            )
            blocked_cats = [
                (f["category"], f["severity"]) for f in blocked_findings
            ]
            self.assertIn(
                ("target_blocked_by_unmigrated_deps", "INFO"),
                blocked_cats,
                blocked_findings,
            )

            os.remove(os.path.join(tmpdir, gn_rel))
            bz_rel = "examples/custom/BUILD.bazel"
            _write_file(
                tmpdir,
                bz_rel,
                HEADER
                + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_binary")\n\n'
                + 'package(default_applicable_licenses = ["//:license"])\n\n'
                + 'rustc_binary(\n    name = "custom",\n    srcs = ["src/main.rs"],\n    edition = "2024",\n)\n',
            )
            del_findings = _run_migration_sanity(
                tmpdir, base, "examples/custom", [gn_rel, bz_rel]
            )
            inc_findings = [
                f
                for f in del_findings
                if f["category"] == "includecode_region_tag_removed"
            ]
            self.assertEqual(len(inc_findings), 1, del_findings)
            self.assertIn("unindexed_tag", inc_findings[0]["message"])

    def test_multi_target_dirs_checks_all_batch_packages(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/lib/unmig/BUILD.gn": HEADER
                    + 'source_set("unmig") {\n  sources = [ "unmig.cc" ]\n}\n',
                    "src/devices/stall/BUILD.gn": HEADER
                    + 'source_set("stall") {\n  deps = [ ":stall_rs" ]\n}\n'
                    + 'kernel_rust_mod("stall_rs") {\n  sources = [ "lib.rs" ]\n}\n',
                    "src/devices/blocked_mod/BUILD.gn": HEADER
                    + 'source_set("blocked_mod") {\n  deps = [ ":mod_rs" ]\n}\n'
                    + 'kernel_rust_mod("mod_rs") {\n'
                    + '  deps = [ "//src/lib/unmig" ]\n'
                    + "}\n",
                    "src/lib/ksync/BUILD.gn": HEADER
                    + 'rustc_library("ksync") {\n'
                    + '  edition = "2024"\n'
                    + '  sources = [ "src/lib.rs" ]\n'
                    + "}\n"
                    + "if (is_kernel) {\n"
                    + '  kernel_rust_mod("ksync_kernel_tests") {\n'
                    + "    testonly = true\n"
                    + '    test_deps = [ ":ksync" ]\n'
                    + "  }\n"
                    + "}\n",
                    "src/lib/unblocked/BUILD.gn": HEADER
                    + 'source_set("unblocked") {\n  sources = [ "u.cc" ]\n}\n',
                },
            )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/devices/stall",
                    "PLANTER_TARGET_DIRS": "src/devices/stall,src/devices/blocked_mod,src/lib/ksync,src/lib/unblocked",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_RESOLVED_CHANGE_BASE": base,
                    "PLANTER_CHANGED_FILES": "",
                }
            )
            out = subprocess.run(
                ["bash", SCRIPT],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            findings = json.loads(out.stdout)
            by_file = {
                (f["file"], f["category"], f["severity"]): f for f in findings
            }
            for pkg_dir, blocker in (
                (
                    "src/devices/stall",
                    "//src/devices/stall:stall_rs (kernel_rust_mod GN-only target)",
                ),
                (
                    "src/devices/blocked_mod",
                    "//src/devices/blocked_mod:mod_rs (kernel_rust_mod GN-only target)",
                ),
                (
                    "src/lib/ksync",
                    "//src/lib/ksync:ksync_kernel_tests (kernel_rust_mod GN-only target)",
                ),
            ):
                blocked_key = (
                    f"{pkg_dir}/BUILD.gn",
                    "target_blocked_by_unmigrated_deps",
                    "INFO",
                )
                self.assertIn(blocked_key, by_file, findings)
                self.assertIn(blocker, by_file[blocked_key]["message"])
                self.assertNotIn(
                    (
                        f"{pkg_dir}/BUILD.gn",
                        "missing_target_bazel_file",
                        "error",
                    ),
                    by_file,
                    findings,
                )
            self.assertIn(
                "//src/lib/unmig",
                by_file[
                    (
                        "src/devices/blocked_mod/BUILD.gn",
                        "target_blocked_by_unmigrated_deps",
                        "INFO",
                    )
                ]["message"],
            )
            self.assertIn(
                (
                    "src/lib/unblocked/BUILD.gn",
                    "missing_target_bazel_file",
                    "error",
                ),
                by_file,
                findings,
            )

    def test_multi_target_dirs_checks_unused_gn_imports_in_unchanged_dirs(
        self,
    ):
        gn_text = (
            HEADER
            + 'import("//build/rust/rustc_library.gni")\n\n'
            + 'group("group") {\n}\n\n'
            + "## BAZEL2GN SENTINEL\n"
        )
        bazel_text = HEADER + 'cc_library(name = "lib")\n'
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/first/BUILD.gn": gn_text,
                    "src/first/BUILD.bazel": bazel_text,
                    "src/second/BUILD.gn": gn_text,
                    "src/second/BUILD.bazel": bazel_text,
                },
            )
            env = {
                k: v
                for k, v in os.environ.items()
                if not k.startswith("PLANTER_")
            }
            env.update(
                {
                    "PLANTER_WORKDIR": tmpdir,
                    "PLANTER_TARGET_DIR": "src/first",
                    "PLANTER_TARGET_DIRS": "src/first,src/second",
                    "PLANTER_CHANGE_BASE": base,
                    "PLANTER_RESOLVED_CHANGE_BASE": base,
                    "PLANTER_CHANGED_FILES": "",
                }
            )
            out = subprocess.run(
                ["bash", SCRIPT],
                env=env,
                capture_output=True,
                text=True,
                check=True,
            )
            findings = json.loads(out.stdout)
            unused_import_files = {
                f["file"]
                for f in findings
                if f["category"] == "unused_gn_import"
            }
            self.assertEqual(
                unused_import_files,
                {"src/first/BUILD.gn", "src/second/BUILD.gn"},
                findings,
            )

    def test_bind_library_synced_below_sentinel_not_flagged_as_unsynced(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            gn_rel = "src/devices/bind/fuchsia.sample/BUILD.gn"
            bz_rel = "src/devices/bind/fuchsia.sample/BUILD.bazel"
            base = _init_repo(
                tmpdir,
                {
                    gn_rel: HEADER
                    + 'import("//build/bind/bind.gni")\n\n'
                    + 'bind_library("fuchsia.sample") {\n'
                    + '  source = "fuchsia.sample.bind"\n'
                    + '  sdk_category = "partner"\n'
                    + '  sdk_area = "Driver SDK"\n'
                    + "}\n",
                },
            )
            _write_file(
                tmpdir,
                bz_rel,
                HEADER
                + 'load("//build/bazel/rules/bind:bind_library.bzl", "bind_library")\n\n'
                + 'package(default_applicable_licenses = ["//:license"])\n\n'
                + "bind_library(\n"
                + '    name = "fuchsia.sample",\n'
                + '    src = "fuchsia.sample.bind",\n'
                + '    api_area = "Driver SDK",\n'
                + '    category = "partner",\n'
                + '    visibility = ["//sdk:__pkg__"],\n'
                + ")\n",
            )
            _write_file(
                tmpdir,
                gn_rel,
                HEADER
                + "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n\n"
                + 'import("//build/tools/bazel2gn/bazel_migration.gni")\n\n'
                + 'bind_library("fuchsia.sample") {\n'
                + '  sdk_area = "Driver SDK"\n'
                + '  sdk_category = "partner"\n'
                + '  source = "fuchsia.sample.bind"\n'
                + '  visibility = [\n    ":*",\n    "//sdk:*",\n  ]\n'
                + "}\n",
            )
            findings = _run_migration_sanity(
                tmpdir,
                base,
                "src/devices/bind/fuchsia.sample",
                [gn_rel, bz_rel],
            )
            cats = [f["category"] for f in findings]
            self.assertNotIn(
                "unnecessary_bazel2gn_on_package_only_bazel", cats, findings
            )

    def test_missing_manual_tag_on_packaged_test(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(tmpdir, {"src/foo/BUILD.gn": HEADER})
            bz_rel = "src/foo/BUILD.bazel"
            _write_file(
                tmpdir,
                bz_rel,
                HEADER
                + """
package(default_applicable_licenses = ["//:license"])

fx_unittest_package(
    name = "pkg",
    unit_tests = [":rust_test", ":rust_test_manual", ":cc_test"],
)

rustc_test(
    name = "rust_test",
    srcs = ["test.rs"],
)

rustc_test(
    name = "rust_test_manual",
    srcs = ["test.rs"],
    tags = ["manual"],
)

fx_cc_test_binary(
    name = "cc_test",
    srcs = ["test.cc"],
)
""",
            )
            findings = _run_migration_sanity(tmpdir, base, "src/foo", [bz_rel])
            cats = [
                (f["category"], f.get("message", ""))
                for f in findings
                if f["category"] == "missing_manual_tag_on_packaged_test"
            ]
            self.assertEqual(len(cats), 1, cats)
            self.assertIn("rust_test", cats[0][1])

    def test_owner_kept_skip_widening_visibility(self):
        base_gn = (
            'shared_library("prebuilt_tbl") {\n'
            '  sources = ["a.cc"]\n'
            '  visibility = [":*"]\n'
            "}\n\n"
            "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n"
        )
        base_bz = (
            HEADER
            + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
            + "# Owner-kept shared_library mirror.\n"
            + "# @bazel2gn:skip\n"
            + "fx_cc_library(\n"
            + '    name = "prebuilt_tbl",\n'
            + '    srcs = ["a.cc"],\n'
            + '    visibility = [":__pkg__"],\n'
            + ")\n"
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/dep/BUILD.gn": base_gn,
                    "src/dep/BUILD.bazel": base_bz,
                    "src/consumer/BUILD.gn": 'source_set("consumer") {}\n',
                },
            )
            os.remove(os.path.join(tmpdir, "src/consumer/BUILD.gn"))
            _write_file(
                tmpdir,
                "src/consumer/BUILD.bazel",
                HEADER
                + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                + 'fx_cc_library(\n    name = "consumer",\n    deps = ["//src/dep:prebuilt_tbl"],\n)\n',
            )
            _write_file(
                tmpdir,
                "src/dep/BUILD.gn",
                'shared_library("prebuilt_tbl") {\n'
                '  sources = ["a.cc"]\n'
                '  visibility = [":*", "//src/consumer:*"]\n'
                "}\n\n"
                "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n",
            )
            _write_file(
                tmpdir,
                "src/dep/BUILD.bazel",
                HEADER
                + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                + "# Owner-kept shared_library mirror.\n"
                + "# @bazel2gn:skip\n"
                + "fx_cc_library(\n"
                + '    name = "prebuilt_tbl",\n'
                + '    srcs = ["a.cc"],\n'
                + '    visibility = [":__pkg__", "//src/consumer:__pkg__"],\n'
                + ")\n",
            )
            findings = _run_migration_sanity(
                tmpdir,
                base,
                "src/consumer",
                [
                    "src/consumer/BUILD.gn",
                    "src/consumer/BUILD.bazel",
                    "src/dep/BUILD.gn",
                    "src/dep/BUILD.bazel",
                ],
            )
            kept = [
                f
                for f in findings
                if f["category"] == "preexisting_owner_skip_kept"
            ]
            new_skip = [
                f for f in findings if f["category"] == "new_bazel2gn_skip"
            ]
            self.assertEqual(len(kept), 1, findings)
            self.assertEqual(new_skip, [], findings)

            # Changing srcs breaks owner_kept_skips.
            _write_file(
                tmpdir,
                "src/dep/BUILD.bazel",
                HEADER
                + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                + "# Owner-kept shared_library mirror.\n"
                + "# @bazel2gn:skip\n"
                + "fx_cc_library(\n"
                + '    name = "prebuilt_tbl",\n'
                + '    srcs = ["a.cc", "b.cc"],\n'
                + '    visibility = [":__pkg__", "//src/consumer:__pkg__"],\n'
                + ")\n",
            )
            findings2 = _run_migration_sanity(
                tmpdir,
                base,
                "src/consumer",
                [
                    "src/consumer/BUILD.gn",
                    "src/consumer/BUILD.bazel",
                    "src/dep/BUILD.gn",
                    "src/dep/BUILD.bazel",
                ],
            )
            kept2 = [
                f
                for f in findings2
                if f["category"] == "preexisting_owner_skip_kept"
            ]
            self.assertEqual(kept2, [], findings2)

    def test_disconnected_zero_rdep_package_migrated_flagged(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    ".gn": "",
                    "bundles/BUILD.gn": (
                        'group("bundles") {\n  deps = ["//src/live"]\n}\n'
                    ),
                    "src/dead/BUILD.gn": 'source_set("dead") {\n  sources = ["a.cc"]\n}\n',
                    "src/live/BUILD.gn": 'source_set("live") {\n  sources = ["b.cc"]\n}\n',
                },
            )
            _write_file(
                tmpdir,
                "src/dead/BUILD.bazel",
                HEADER
                + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                + 'fx_cc_library(\n    name = "dead",\n    srcs = ["a.cc"],\n)\n',
            )
            _write_file(
                tmpdir,
                "src/dead/BUILD.gn",
                "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n",
            )
            findings = _run_migration_sanity(
                tmpdir,
                base,
                "src/dead",
                ["src/dead/BUILD.bazel", "src/dead/BUILD.gn"],
            )
            hits = [
                f
                for f in findings
                if f["category"] == "disconnected_zero_rdep_package_migrated"
                and f["severity"] == "error"
            ]
            self.assertEqual(len(hits), 1, findings)

            _write_file(
                tmpdir,
                "src/live/BUILD.bazel",
                HEADER
                + 'load("//build/bazel/rules/cc:defs.bzl", "fx_cc_library")\n\n'
                + 'fx_cc_library(\n    name = "live",\n    srcs = ["b.cc"],\n    visibility = ["//bundles:__pkg__"],\n)\n',
            )
            _write_file(
                tmpdir,
                "src/live/BUILD.gn",
                "## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##\n",
            )
            findings_live = _run_migration_sanity(
                tmpdir,
                base,
                "src/live",
                ["src/live/BUILD.bazel", "src/live/BUILD.gn"],
            )
            hits_live = [
                f
                for f in findings_live
                if f["category"] == "disconnected_zero_rdep_package_migrated"
            ]
            self.assertEqual(hits_live, [], findings_live)

    def test_ancillary_preexisting_targets_exempted(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            base = _init_repo(
                tmpdir,
                {
                    "src/upstream/BUILD.bazel": (
                        HEADER
                        + 'load("//build/bazel/rules/packages:defs.bzl", "fx_package")\n'
                        + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_test")\n\n'
                        + "rustc_test(\n"
                        + '    name = "bin_test",\n'
                        + '    srcs = ["lib.rs"],\n'
                        + ")\n\n"
                        + "fx_package(\n"
                        + '    name = "upstream",\n'
                        + '    test_components = [":bin_comp"],\n'
                        + ")\n\n"
                        + "fx_package(\n"
                        + '    name = "package-debug",\n'
                        + '    visibility = ["//src/other:__pkg__"],\n'
                        + ")\n"
                    ),
                    "src/upstream/BUILD.gn": (
                        HEADER
                        + 'fuchsia_package("upstream") {\n'
                        + '  deps = [ ":bin_comp" ]\n'
                        + "}\n"
                    ),
                    "src/caller/BUILD.gn": (HEADER + 'group("caller") {\n}\n'),
                },
            )
            _write_file(
                tmpdir,
                "src/upstream/BUILD.bazel",
                HEADER
                + 'load("//build/bazel/rules/packages:defs.bzl", "fx_package")\n'
                + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_test")\n\n'
                + "rustc_test(\n"
                + '    name = "bin_test",\n'
                + '    srcs = ["lib.rs"],\n'
                + ")\n\n"
                + "fx_package(\n"
                + '    name = "upstream",\n'
                + '    test_components = [":bin_comp"],\n'
                + ")\n\n"
                + "fx_package(\n"
                + '    name = "package-debug",\n'
                + "    visibility = [\n"
                + '        "//src/caller:__pkg__",\n'
                + '        "//src/other:__pkg__",\n'
                + "    ],\n"
                + ")\n",
            )
            _write_file(
                tmpdir,
                "src/caller/BUILD.bazel",
                HEADER
                + 'load("//build/bazel/rules/packages:defs.bzl", "fx_package")\n'
                + 'load("//build/bazel/rules/rust:defs.bzl", "rustc_test")\n'
                + 'load("//build/bazel/rules/testing:defs.bzl", "fx_unittest_package")\n\n'
                + "rustc_test(\n"
                + '    name = "caller_test",\n'
                + '    srcs = ["caller.rs"],\n'
                + ")\n\n"
                + "fx_unittest_package(\n"
                + '    name = "caller_tests",\n'
                + '    unit_tests = [":caller_test"],\n'
                + ")\n",
            )
            findings = _run_migration_sanity(
                tmpdir,
                base,
                "src/caller",
                ["src/caller/BUILD.bazel", "src/upstream/BUILD.bazel"],
            )
            ancillary_hits = [
                f
                for f in findings
                if f.get("file") == "src/upstream/BUILD.bazel"
                or f.get("category") == "duplicate_gn_fuchsia_package"
            ]
            self.assertEqual(ancillary_hits, [], findings)
            caller_manual = [
                f
                for f in findings
                if f.get("file") == "src/caller/BUILD.bazel"
                and f.get("category") == "missing_manual_tag_on_packaged_test"
            ]
            self.assertEqual(len(caller_manual), 1, findings)


if __name__ == "__main__":
    unittest.main()
