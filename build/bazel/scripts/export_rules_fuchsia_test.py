#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import pathlib
import sys
import tempfile
import unittest
from unittest import mock

import export_rules_fuchsia


class ExportRulesFuchsiaTest(unittest.TestCase):
    def test_copy_tree_excludes_agents_skills_and_readmes(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = pathlib.Path(tmpdir)
            src = root / "src"
            dst = root / "dst"

            (src / "subdir").mkdir(parents=True)
            (src / "subdir/rules.bzl").write_text("# rules")
            (src / "README.md").write_text("# top readme")
            (src / "subdir/README.md").write_text("# sub readme")
            (src / ".agents/skills/my-skill").mkdir(parents=True)
            (src / ".agents/skills/my-skill/SKILL.md").write_text("skill")
            (src / "subdir/.agents/skills/nested-skill").mkdir(parents=True)
            (src / "subdir/.agents/skills/nested-skill/SKILL.md").write_text(
                "nested skill"
            )
            (src / "skills/other-skill").mkdir(parents=True)
            (src / "skills/other-skill/SKILL.md").write_text("other")
            (src / "subdir/skills/nested").mkdir(parents=True)
            (src / "subdir/skills/nested/file.txt").write_text("nested")

            inputs = export_rules_fuchsia.copy_tree(src, dst)

            self.assertEqual(inputs, {src / "subdir/rules.bzl"})
            self.assertTrue((dst / "subdir/rules.bzl").is_file())
            self.assertFalse((dst / "README.md").exists())
            self.assertFalse((dst / "subdir/README.md").exists())
            self.assertFalse((dst / ".agents").exists())
            self.assertFalse((dst / "subdir/.agents").exists())
            self.assertFalse((dst / "skills").exists())
            self.assertFalse((dst / "subdir/skills").exists())

    def test_main_excludes_in_tree_docs_and_skills_from_export_and_depfile(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            root = pathlib.Path(tmpdir)
            fuchsia_dir = root / "fuchsia"
            sdk_dir = fuchsia_dir / "build/bazel_sdk/bazel_rules_fuchsia"
            common_dir = fuchsia_dir / "build/bazel_sdk/fuchsia_rules_common"
            output_dir = root / "out/rules_fuchsia"
            depfile = root / "out/rules_fuchsia.d"
            stamp = root / "out/rules_fuchsia.stamp"

            sdk_dir.mkdir(parents=True)
            (sdk_dir / "MODULE.bazel").write_text(
                'module(name = "rules_fuchsia")\n'
                "# BEGIN_FUCHSIA_IN_TREE_ONLY\n"
                'bazel_dep(name = "fuchsia_rules_common")\n'
                "# END_FUCHSIA_IN_TREE_ONLY\n"
            )
            (sdk_dir / "README.md").write_text("# sdk readme")
            (sdk_dir / "fuchsia/defs.bzl").parent.mkdir(parents=True)
            (sdk_dir / "fuchsia/defs.bzl").write_text(
                'load("@fuchsia_rules_common//bind:bindc.bzl", "bind")\n'
            )
            (sdk_dir / ".agents/skills/sdk-skill").mkdir(parents=True)
            (sdk_dir / ".agents/skills/sdk-skill/SKILL.md").write_text("sdk")

            (common_dir / "bind").mkdir(parents=True)
            (common_dir / "bind/bindc.bzl").write_text("bind = 1\n")
            (common_dir / "README.md").write_text("# common readme")
            (common_dir / ".agents/skills/common-skill").mkdir(parents=True)
            (common_dir / ".agents/skills/common-skill/SKILL.md").write_text(
                "common"
            )
            (common_dir / "skills/extra").mkdir(parents=True)
            (common_dir / "skills/extra/SKILL.md").write_text("extra")

            argv = [
                "export_rules_fuchsia.py",
                "--fuchsia-dir",
                str(fuchsia_dir),
                "--output-dir",
                str(output_dir),
                "--depfile",
                str(depfile),
                "--stamp",
                str(stamp),
            ]
            with mock.patch.object(sys, "argv", argv):
                rc = export_rules_fuchsia.main()

            self.assertEqual(rc, 0)
            self.assertTrue((output_dir / "fuchsia/defs.bzl").is_file())
            self.assertTrue(
                (output_dir / "fuchsia_rules_common/bind/bindc.bzl").is_file()
            )
            self.assertIn(
                "@rules_fuchsia//fuchsia_rules_common/bind:bindc.bzl",
                (output_dir / "fuchsia/defs.bzl").read_text(),
            )
            self.assertFalse((output_dir / "README.md").exists())
            self.assertFalse((output_dir / ".agents").exists())
            self.assertFalse(
                (output_dir / "fuchsia_rules_common/README.md").exists()
            )
            self.assertFalse(
                (output_dir / "fuchsia_rules_common/.agents").exists()
            )
            self.assertFalse(
                (output_dir / "fuchsia_rules_common/skills").exists()
            )

            depfile_text = depfile.read_text()
            self.assertNotIn("README.md", depfile_text)
            self.assertNotIn(".agents", depfile_text)
            self.assertNotIn("skills", depfile_text)
            self.assertTrue(stamp.is_file())


if __name__ == "__main__":
    unittest.main()
