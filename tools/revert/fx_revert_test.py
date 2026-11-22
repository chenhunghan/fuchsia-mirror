# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Smoke tests for the `fx revert` host tool bundle (`//tools/revert:fx_revert`)."""

import os
import subprocess
import unittest
from pathlib import Path


def _find_tool_launcher() -> Path:
    rel = "tools/revert/fx_revert/revert"
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
    raise FileNotFoundError(f"Could not find revert launcher in {candidates}")


class RevertHostToolTests(unittest.TestCase):
    def test_bundles_compiled_revert_binary(self) -> None:
        launcher = _find_tool_launcher()
        runtime_dir = launcher.parent / "revert.runtime_dir"
        revert_bin = runtime_dir / "revert"
        self.assertTrue(
            revert_bin.is_file(), msg=f"Missing bundled revert: {revert_bin}"
        )
        self.assertTrue(os.access(revert_bin, os.X_OK))
        self.assertFalse(revert_bin.is_symlink())
        self.assertTrue(
            revert_bin.resolve().is_relative_to(runtime_dir.resolve())
        )

    def test_sources_manifest_includes_tool_sources(self) -> None:
        launcher = _find_tool_launcher()
        runtime_dir = launcher.parent / "revert.runtime_dir"
        sources_manifest = runtime_dir / "sources.manifest"
        packages_manifest = runtime_dir / "packages.manifest"
        self.assertTrue(sources_manifest.is_file())
        self.assertTrue(packages_manifest.is_file())

        sources_text = sources_manifest.read_text(encoding="utf-8")
        self.assertIn("tools/revert/src/main.rs", sources_text)
        self.assertIn("tools/revert/src/revert.rs", sources_text)
        self.assertNotIn("third_party/", sources_text)

        packages_text = packages_manifest.read_text(encoding="utf-8")
        self.assertIn("tools/revert/BUILD.bazel", packages_text)
        self.assertNotIn("third_party/", packages_text)

    def test_help_flag(self) -> None:
        launcher = _find_tool_launcher()
        proc = subprocess.run(
            [str(launcher), "--help"],
            env={"PATH": "/usr/bin:/bin"},
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, msg=proc.stderr)
        self.assertIn("Usage: revert", proc.stdout)
        self.assertIn("--dry-run", proc.stdout)


if __name__ == "__main__":
    unittest.main()
