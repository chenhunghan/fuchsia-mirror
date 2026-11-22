#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for post_processing_python_wrapper.sh."""

import os
import pathlib
import pkgutil
import subprocess
import sys
import tempfile
import unittest

_WRAPPER_PATH = (
    pathlib.Path(__file__).parent / "post_processing_python_wrapper.sh"
)


def _load_wrapper() -> bytes:
    """Loads the wrapper script.

    Supports both data packaged into the `host_py_test()` archive (flattened to
    basenames) and running this file directly from the source tree.
    """
    try:
        data = pkgutil.get_data(__name__, _WRAPPER_PATH.name)
        if data is not None:
            return data
    except OSError:
        pass
    return _WRAPPER_PATH.read_bytes()


# A fake post-processing stage: appends `+<tag>` to the input image and logs
# its non -z/-o args.
_FAKE_STAGE = """
import pathlib, sys
args = sys.argv[1:]
inp = pathlib.Path(args[args.index("-z") + 1])
out = pathlib.Path(args[args.index("-o") + 1])
tag = args[args.index("--tag") + 1]
if tag == "fail":
    sys.exit(3)
out.write_bytes(inp.read_bytes() + b"+" + tag.encode())
with open("stage_log.txt", "a") as log:
    log.write(" ".join(a for a in args if a not in (str(inp), str(out))) + "\\n")
"""

# A fake in-place stage, e.g. `avbtool.py add_hash_footer --image <image>`:
# appends `+<tag>` to `--image`.
_FAKE_IN_PLACE_STAGE = """
import argparse, pathlib
parser = argparse.ArgumentParser()
parser.add_argument("--image", required=True, type=pathlib.Path)
parser.add_argument("--tag", required=True)
args = parser.parse_args()
args.image.write_bytes(args.image.read_bytes() + b"+" + args.tag.encode())
"""


class PostProcessingPythonWrapperTests(unittest.TestCase):
    def setUp(self) -> None:
        temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(temp_dir.cleanup)
        root = pathlib.Path(temp_dir.name)

        # Board-like directory holding the wrapper and its inputs; the wrapper
        # `cd`s here and resolves the interpreter relative to it.
        self.board = root / "board"
        self.board.mkdir()
        (self.board / "wrapper.sh").write_bytes(_load_wrapper())
        os.symlink(sys.executable, self.board / "python3")
        (self.board / "stage.py").write_text(_FAKE_STAGE)
        (self.board / "in_place.py").write_text(_FAKE_IN_PLACE_STAGE)

        # Assembly runs from a different cwd with cwd-relative -z/-o paths.
        self.cwd = root / "assembly"
        self.cwd.mkdir()
        (self.cwd / "in.zbi").write_bytes(b"zbi")

    def _run(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(self.board / "wrapper.sh"), "-p", "python3", *args]
            + ["-z", "in.zbi", "-o", "out.img"],
            cwd=self.cwd,
            capture_output=True,
            text=True,
        )

    def _stage_log(self) -> list[str]:
        return (self.board / "stage_log.txt").read_text().splitlines()

    def test_single_stage(self) -> None:
        result = self._run("stage.py", "--tag", "a")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.cwd / "out.img").read_bytes(), b"zbi+a")
        self.assertEqual(self._stage_log(), ["--tag a -z -o"])

    def test_chained_stages(self) -> None:
        result = self._run(
            "stage.py", "--tag", "a", "--then", "stage.py", "--tag", "b"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.cwd / "out.img").read_bytes(), b"zbi+a+b")
        self.assertEqual(
            self._stage_log(),
            ["--tag a -z -o", "--tag b -z -o"],
        )
        # Intermediates must not be left in the board directory.
        self.assertEqual(
            sorted(p.name for p in self.board.iterdir()),
            [
                "in_place.py",
                "python3",
                "stage.py",
                "stage_log.txt",
                "wrapper.sh",
            ],
        )

    def test_in_place_stage(self) -> None:
        result = self._run(
            "stage.py",
            "--tag",
            "a",
            "--then",
            "in_place.py",
            "--image",
            "@IMAGE@",
            "--tag",
            "b",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.cwd / "out.img").read_bytes(), b"zbi+a+b")

    def test_in_place_first_stage_does_not_modify_zbi(self) -> None:
        result = self._run("in_place.py", "--image=@IMAGE@", "--tag", "a")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.cwd / "in.zbi").read_bytes(), b"zbi")
        self.assertEqual((self.cwd / "out.img").read_bytes(), b"zbi+a")

    def test_failed_stage_stops_chain(self) -> None:
        result = self._run(
            "stage.py", "--tag", "fail", "--then", "stage.py", "--tag", "b"
        )

        self.assertEqual(result.returncode, 3)
        self.assertFalse((self.cwd / "out.img").exists())
        self.assertFalse((self.board / "stage_log.txt").exists())

    def test_empty_stage_is_rejected(self) -> None:
        for args in [
            ("stage.py", "--tag", "a", "--then"),
            ("--then", "stage.py", "--tag", "a"),
            ("stage.py", "--tag", "a", "--then", "--then", "stage.py"),
        ]:
            with self.subTest(args=args):
                result = self._run(*args)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("empty stage", result.stderr)


if __name__ == "__main__":
    unittest.main()
