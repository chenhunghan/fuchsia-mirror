# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import subprocess
import unittest
from pathlib import Path


def _find_tool_launcher() -> Path:
    rel = "tools/cmc/fx_cmc/cmc"
    candidates = [
        Path(rel),
        Path("_main") / rel,
    ]
    runfiles_dir = os.environ.get("RUNFILES_DIR")
    if runfiles_dir:
        candidates.insert(0, Path(runfiles_dir) / "_main" / rel)
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    if not os.environ.get("TEST_SRCDIR") and not runfiles_dir:
        raise unittest.SkipTest(
            f"Bazel launcher not in runfiles ({candidates})"
        )
    raise FileNotFoundError(f"Could not find cmc launcher in {candidates}")


class CmcHostToolTests(unittest.TestCase):
    def test_bundles_compiled_cmc_binary(self) -> None:
        launcher = _find_tool_launcher()
        runtime_dir = launcher.parent / "cmc.runtime_dir"
        cmc_bin = runtime_dir / "cmc"
        self.assertTrue(
            cmc_bin.is_file(), msg=f"Missing bundled cmc: {cmc_bin}"
        )
        self.assertTrue(os.access(cmc_bin, os.X_OK))
        self.assertFalse(cmc_bin.is_symlink())
        self.assertTrue(cmc_bin.resolve().is_relative_to(runtime_dir.resolve()))

    def test_sources_and_packages_manifest_include_transitive_deps(
        self,
    ) -> None:
        launcher = _find_tool_launcher()
        runtime_dir = launcher.parent / "cmc.runtime_dir"
        sources_manifest = runtime_dir / "sources.manifest"
        packages_manifest = runtime_dir / "packages.manifest"
        self.assertTrue(sources_manifest.is_file())
        self.assertTrue(packages_manifest.is_file())

        sources_text = sources_manifest.read_text(encoding="utf-8")
        self.assertIn("tools/cmc/src/main.rs", sources_text)
        self.assertIn("tools/lib/cml/src/lib.rs", sources_text)
        self.assertIn("sdk/fidl/fuchsia.component.decl/", sources_text)
        self.assertNotIn("build/bazel/scripts/runfiles_utils.py", sources_text)
        self.assertNotIn("third_party/", sources_text)

        packages_text = packages_manifest.read_text(encoding="utf-8")
        self.assertIn("tools/cmc/BUILD.bazel", packages_text)
        self.assertIn("tools/lib/cml/BUILD.bazel", packages_text)
        self.assertIn(
            "sdk/fidl/fuchsia.component.decl/BUILD.bazel", packages_text
        )
        self.assertNotIn("third_party/", packages_text)

    def test_help_flags(self) -> None:
        launcher = _find_tool_launcher()
        for flag in ("-h", "--help", "help"):
            with self.subTest(flag=flag):
                proc = subprocess.run(
                    [str(launcher), flag],
                    env={"PATH": "/usr/bin:/bin"},
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(proc.returncode, 0, msg=proc.stderr)
                self.assertIn("compile", proc.stdout)
                self.assertIn("format", proc.stdout)


if __name__ == "__main__":
    unittest.main()
