# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import os
import subprocess
import tempfile
import unittest
from pathlib import Path


def _find_tool_binary() -> Path:
    candidates = [
        Path("tools/devshell/host_tools/get_src_dir/get-src-dir"),
        Path("_main/tools/devshell/host_tools/get_src_dir/get-src-dir"),
    ]
    runfiles_dir = os.environ.get("RUNFILES_DIR")
    if runfiles_dir:
        candidates.insert(
            0,
            Path(runfiles_dir)
            / "_main"
            / "tools/devshell/host_tools/get_src_dir/get-src-dir",
        )
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    # Also fallback to direct script if testing outside Bazel
    script = Path("tools/devshell/host_tools/get_src_dir/get_src_dir.sh")
    if script.is_file():
        return script.resolve()
    raise FileNotFoundError(f"Could not find get-src-dir in {candidates}")


class GetSrcDirShellToolTests(unittest.TestCase):
    def test_prints_fuchsia_dir_from_env(self) -> None:
        tool = _find_tool_binary()
        if (tool.parent / "get-src-dir.runtime_dir").is_dir():
            self.assertIn(
                "Copyright 2026 The Fuchsia Authors.",
                (
                    tool.parent / "get-src-dir.runtime_dir" / "get_src_dir.sh"
                ).read_text(encoding="utf-8"),
            )
        with tempfile.TemporaryDirectory() as td:
            fake_root = Path(td) / "fuchsia_root"
            nested_cwd = fake_root / "src" / "lib"
            nested_cwd.mkdir(parents=True)

            env = os.environ.copy()
            env["FUCHSIA_DIR"] = str(fake_root)
            proc = subprocess.run(
                [str(tool)],
                cwd=nested_cwd,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, msg=proc.stderr)
            self.assertEqual(proc.stdout, f"{fake_root}\n")

    def test_rejects_unexpected_arguments(self) -> None:
        tool = _find_tool_binary()
        env = os.environ.copy()
        env["FUCHSIA_DIR"] = "/tmp/fake_fuchsia"
        proc = subprocess.run(
            [str(tool), "--unexpected"],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertNotEqual(proc.returncode, 0)


if __name__ == "__main__":
    unittest.main()
