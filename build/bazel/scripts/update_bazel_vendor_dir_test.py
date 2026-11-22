#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import tempfile
import unittest
from pathlib import Path
from typing import Any
from unittest import mock

import update_bazel_vendor_dir


class UpdateBazelVendorDirTest(unittest.TestCase):
    def test_merge_bcr_registry(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            bcr1 = td_path / "bcr1"
            bcr2 = td_path / "bcr2"
            dst_bcr = td_path / "dst_bcr"

            # Workspace 1 selects foo@1.0 (has MODULE.bazel + source.json)
            # and evaluates bar@1.0 as a candidate (only MODULE.bazel).
            (bcr1 / "modules/foo/1.0").mkdir(parents=True)
            (bcr1 / "modules/foo/1.0/MODULE.bazel").write_text("foo 1.0")
            (bcr1 / "modules/foo/1.0/source.json").write_text('{"foo": "1.0"}')
            (bcr1 / "modules/bar/1.0").mkdir(parents=True)
            (bcr1 / "modules/bar/1.0/MODULE.bazel").write_text("bar 1.0")
            (bcr1 / "bazel_registry.json").write_text("{}")

            # Workspace 2 evaluates foo@1.0 as a candidate (only MODULE.bazel),
            # selects bar@1.0 (has MODULE.bazel + source.json), and selects baz@2.0.
            (bcr2 / "modules/foo/1.0").mkdir(parents=True)
            (bcr2 / "modules/foo/1.0/MODULE.bazel").write_text("foo 1.0")
            (bcr2 / "modules/bar/1.0").mkdir(parents=True)
            (bcr2 / "modules/bar/1.0/MODULE.bazel").write_text("bar 1.0")
            (bcr2 / "modules/bar/1.0/source.json").write_text('{"bar": "1.0"}')
            (bcr2 / "modules/baz/2.0").mkdir(parents=True)
            (bcr2 / "modules/baz/2.0/MODULE.bazel").write_text("baz 2.0")
            (bcr2 / "modules/baz/2.0/source.json").write_text('{"baz": "2.0"}')
            (bcr2 / "bazel_registry.json").write_text("{}")

            update_bazel_vendor_dir.merge_bcr_registry(bcr1, dst_bcr)
            update_bazel_vendor_dir.merge_bcr_registry(bcr2, dst_bcr)

            self.assertTrue((dst_bcr / "bazel_registry.json").is_file())
            self.assertTrue(
                (dst_bcr / "modules/foo/1.0/MODULE.bazel").is_file()
            )
            self.assertTrue((dst_bcr / "modules/foo/1.0/source.json").is_file())
            self.assertTrue(
                (dst_bcr / "modules/bar/1.0/MODULE.bazel").is_file()
            )
            self.assertTrue((dst_bcr / "modules/bar/1.0/source.json").is_file())
            self.assertTrue(
                (dst_bcr / "modules/baz/2.0/MODULE.bazel").is_file()
            )
            self.assertTrue((dst_bcr / "modules/baz/2.0/source.json").is_file())

    def test_multi_workspace_vendor_and_registry_merge(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            ws1 = td_path / "ws1"
            ws2 = td_path / "ws2"
            ws1.mkdir()
            ws2.mkdir()
            bazel_bin = td_path / "bazel"
            bazel_bin.touch()

            vendor_dir_1 = td_path / "vendor/fuchsia_in_tree"
            vendor_dir_2 = td_path / "vendor/bazel_sdk_oot"
            registry_dir = td_path / "registries/bcr.bazel.build"

            # Pre-populate a stale module in registry_dir that should be cleaned up
            # by the first (non-merging) vendor call.
            (registry_dir / "modules/stale_mod/0.1").mkdir(parents=True)
            (registry_dir / "modules/stale_mod/0.1/MODULE.bazel").write_text(
                "stale"
            )

            # Pre-populate stub repo in vendor_dir_1.
            (vendor_dir_1 / "apple_support+").mkdir(parents=True)
            (vendor_dir_1 / "apple_support+/BUILD.bazel").write_text(
                "# stubbed"
            )

            def fake_check_call(cmd: list[Any], **kwargs: Any) -> int:
                vendor_arg = [
                    str(a) for a in cmd if str(a).startswith("--vendor_dir=")
                ][0]
                out_dir = Path(vendor_arg.split("=", 1)[1])
                bazel_external = out_dir / "bazel-external"
                if not bazel_external.is_symlink():
                    bazel_external.symlink_to(
                        "/nonexistent/output_base/external"
                    )
                cwd = kwargs.get("cwd")
                if cwd == ws1:
                    (out_dir / "rules_rust+/bin").mkdir(
                        parents=True, exist_ok=True
                    )
                    (out_dir / "rules_rust+/BUILD.bazel").write_text("rust")
                    (out_dir / "rules_rust+/bin/cargo-bazel").write_text(
                        "binary"
                    )
                    (
                        out_dir
                        / "_registries/bcr.bazel.build/modules/rules_rust/0.74.0"
                    ).mkdir(parents=True, exist_ok=True)
                    (
                        out_dir
                        / "_registries/bcr.bazel.build/modules/rules_rust/0.74.0/MODULE.bazel"
                    ).write_text("rules_rust")
                elif cwd == ws2:
                    (out_dir / "rules_cc+").mkdir(parents=True, exist_ok=True)
                    (out_dir / "rules_cc+/BUILD.bazel").write_text("cc")
                    (
                        out_dir
                        / "_registries/bcr.bazel.build/modules/rules_cc/0.2.19"
                    ).mkdir(parents=True, exist_ok=True)
                    (
                        out_dir
                        / "_registries/bcr.bazel.build/modules/rules_cc/0.2.19/MODULE.bazel"
                    ).write_text("rules_cc")
                return 0

            with mock.patch(
                "subprocess.check_call", side_effect=fake_check_call
            ):
                # 1. Update first workspace (replaces registry cleanly)
                rc1 = update_bazel_vendor_dir.main(
                    [
                        f"--bazel={bazel_bin}",
                        f"--workspace={ws1}",
                        f"--bazel-vendor-dir={vendor_dir_1}",
                        f"--bazel-registry-dir={registry_dir}",
                        "--repo=@@rules_rust+",
                        "--stub-repo=@@apple_support+",
                        "--exclude=rules_rust+/*/cargo-bazel",
                    ]
                )
                self.assertEqual(rc1, 0)

                # 2. Update second workspace (merges into registry)
                rc2 = update_bazel_vendor_dir.main(
                    [
                        f"--bazel={bazel_bin}",
                        f"--workspace={ws2}",
                        f"--bazel-vendor-dir={vendor_dir_2}",
                        f"--bazel-registry-dir={registry_dir}",
                        "--merge-registry",
                        "--repo=@@rules_cc+",
                        "--exclude=rules_rust+/*/cargo-bazel",
                    ]
                )
                self.assertEqual(rc2, 0)

            # Verify vendor_dir_1
            self.assertTrue(
                (vendor_dir_1 / "rules_rust+/BUILD.bazel").is_file()
            )
            self.assertFalse(
                (vendor_dir_1 / "rules_rust+/bin/cargo-bazel").exists()
            )
            self.assertTrue(
                (vendor_dir_1 / "apple_support+/BUILD.bazel").is_file()
            )
            self.assertFalse((vendor_dir_1 / "_registries").exists())
            self.assertFalse((vendor_dir_1 / "bazel-external").is_symlink())
            vendor_bazel_1 = (vendor_dir_1 / "VENDOR.bazel").read_text()
            self.assertIn('pin("@@apple_support+")', vendor_bazel_1)
            self.assertIn('pin("@@rules_rust+")', vendor_bazel_1)

            # Verify vendor_dir_2
            self.assertTrue((vendor_dir_2 / "rules_cc+/BUILD.bazel").is_file())
            self.assertFalse((vendor_dir_2 / "_registries").exists())
            self.assertFalse((vendor_dir_2 / "bazel-external").is_symlink())
            vendor_bazel_2 = (vendor_dir_2 / "VENDOR.bazel").read_text()
            self.assertIn('pin("@@rules_cc+")', vendor_bazel_2)

            # Verify registry_dir has merged both and dropped stale_mod
            self.assertFalse((registry_dir / "modules/stale_mod").exists())
            self.assertTrue(
                (
                    registry_dir / "modules/rules_rust/0.74.0/MODULE.bazel"
                ).is_file()
            )
            self.assertTrue(
                (
                    registry_dir / "modules/rules_cc/0.2.19/MODULE.bazel"
                ).is_file()
            )

    def test_deduplication_and_check_licenses_readmes_sync(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            td_path = Path(td)
            ws1 = td_path / "ws1"
            ws2 = td_path / "ws2"
            ws3 = td_path / "ws3"
            ws1.mkdir()
            ws2.mkdir()
            ws3.mkdir()
            bazel_bin = td_path / "bazel"
            bazel_bin.touch()

            vendor_base = td_path / "third_party/bazel_vendor"
            vendor_in_tree = vendor_base / "fuchsia_in_tree"
            vendor_sdk_in_tree = vendor_base / "bazel_sdk_in_tree"
            vendor_sdk_oot = vendor_base / "bazel_sdk_oot"
            registry_dir = (
                td_path / "third_party/bazel_registries/bcr.bazel.build"
            )

            readmes_base = (
                td_path
                / "tools/check-licenses/assets/readmes/third_party/bazel_vendor"
            )
            for repo in ("rules_python+", "rules_cc+"):
                (readmes_base / "fuchsia_in_tree" / repo).mkdir(parents=True)
                (
                    readmes_base / "fuchsia_in_tree" / repo / "README.fuchsia"
                ).write_text(f"Name: {repo}\n")
            # Pre-populate a redundant readme in bazel_sdk_oot that should be
            # removed once deduplicated.
            (readmes_base / "bazel_sdk_oot/rules_python+").mkdir(parents=True)
            (
                readmes_base / "bazel_sdk_oot/rules_python+/README.fuchsia"
            ).write_text("Name: rules_python+\n")

            # Pre-populate stub repo in fuchsia_in_tree with an in-tree README.fuchsia.
            (vendor_in_tree / "apple_support+").mkdir(parents=True)
            (vendor_in_tree / "apple_support+/BUILD.bazel").write_text(
                "# stubbed"
            )
            (vendor_in_tree / "apple_support+/README.fuchsia").write_text(
                "Name: apple_support+\n"
            )

            def fake_check_call(cmd: list[Any], **kwargs: Any) -> int:
                vendor_arg = [
                    str(a) for a in cmd if str(a).startswith("--vendor_dir=")
                ][0]
                out_dir = Path(vendor_arg.split("=", 1)[1])
                cwd = kwargs.get("cwd")
                # rules_python+ is identical across all 3 workspaces.
                (out_dir / "rules_python+").mkdir(parents=True, exist_ok=True)
                (out_dir / "rules_python+/BUILD.bazel").write_text("py_v1")
                if cwd == ws1:
                    (out_dir / "rules_cc+").mkdir(parents=True, exist_ok=True)
                    (out_dir / "rules_cc+/BUILD.bazel").write_text("cc_patched")
                elif cwd in (ws2, ws3):
                    # rules_cc+ is identical between ws2 and ws3, but differs from ws1.
                    (out_dir / "rules_cc+").mkdir(parents=True, exist_ok=True)
                    (out_dir / "rules_cc+/BUILD.bazel").write_text(
                        "cc_unpatched"
                    )
                return 0

            with mock.patch(
                "subprocess.check_call", side_effect=fake_check_call
            ):
                for ws, vdir in (
                    (ws1, vendor_in_tree),
                    (ws2, vendor_sdk_in_tree),
                    (ws3, vendor_sdk_oot),
                ):
                    args = [
                        f"--bazel={bazel_bin}",
                        f"--workspace={ws}",
                        f"--bazel-vendor-dir={vdir}",
                        f"--bazel-registry-dir={registry_dir}",
                        "--repo=@@rules_python+",
                        "--repo=@@rules_cc+",
                        "--stub-repo=@@apple_support+",
                    ]
                    self.assertEqual(update_bazel_vendor_dir.main(args), 0)

                # Re-run on vendor_sdk_oot when symlinks already exist to verify
                # idempotency (including symlinked stub repos).
                self.assertEqual(
                    update_bazel_vendor_dir.main(
                        [
                            f"--bazel={bazel_bin}",
                            f"--workspace={ws3}",
                            f"--bazel-vendor-dir={vendor_sdk_oot}",
                            f"--bazel-registry-dir={registry_dir}",
                            "--repo=@@rules_python+",
                            "--repo=@@rules_cc+",
                            "--stub-repo=@@apple_support+",
                        ]
                    ),
                    0,
                )

            # 1. fuchsia_in_tree has only real directories.
            for repo in ("rules_python+", "rules_cc+", "apple_support+"):
                p = vendor_in_tree / repo
                self.assertTrue(p.is_dir() and not p.is_symlink(), repo)

            # 2. bazel_sdk_in_tree symlinks rules_python+ and apple_support+ to
            #    ../fuchsia_in_tree, and keeps rules_cc+ as a real directory.
            self.assertEqual(
                (vendor_sdk_in_tree / "rules_python+").readlink(),
                Path("../fuchsia_in_tree/rules_python+"),
            )
            self.assertEqual(
                (vendor_sdk_in_tree / "apple_support+").readlink(),
                Path("../fuchsia_in_tree/apple_support+"),
            )
            self.assertTrue(
                (vendor_sdk_in_tree / "rules_cc+").is_dir()
                and not (vendor_sdk_in_tree / "rules_cc+").is_symlink()
            )

            # 3. bazel_sdk_oot symlinks rules_python+ and apple_support+ to
            #    ../fuchsia_in_tree, and rules_cc+ to ../bazel_sdk_in_tree.
            self.assertEqual(
                (vendor_sdk_oot / "rules_python+").readlink(),
                Path("../fuchsia_in_tree/rules_python+"),
            )
            self.assertEqual(
                (vendor_sdk_oot / "apple_support+").readlink(),
                Path("../fuchsia_in_tree/apple_support+"),
            )
            self.assertEqual(
                (vendor_sdk_oot / "rules_cc+").readlink(),
                Path("../bazel_sdk_in_tree/rules_cc+"),
            )

            # 4. Verify check-licenses readmes synchronization:
            #    - bazel_sdk_in_tree/rules_cc+/README.fuchsia was copied from fuchsia_in_tree
            #    - bazel_sdk_in_tree/rules_python+ does not exist (symlinked)
            #    - bazel_sdk_oot directory was pruned completely (all repos symlinked)
            self.assertTrue(
                (
                    readmes_base / "bazel_sdk_in_tree/rules_cc+/README.fuchsia"
                ).is_file()
            )
            self.assertEqual(
                (
                    readmes_base / "bazel_sdk_in_tree/rules_cc+/README.fuchsia"
                ).read_text(),
                (
                    readmes_base / "fuchsia_in_tree/rules_cc+/README.fuchsia"
                ).read_text(),
            )
            self.assertFalse(
                (readmes_base / "bazel_sdk_in_tree/rules_python+").exists()
            )
            self.assertFalse((readmes_base / "bazel_sdk_oot").exists())


if __name__ == "__main__":
    unittest.main()
