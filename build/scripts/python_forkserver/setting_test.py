# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.scripts.python_forkserver.setting."""

import pathlib
import tempfile
import unittest

from build.scripts.python_forkserver import setting as forkserver_setting

_ENV = forkserver_setting._ENV_PYTHON_FORKSERVER


class LoadTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.source_dir = pathlib.Path(self._tmp.name)

    def _write_config(self, content: str) -> None:
        config_dir = self.source_dir / ".fx" / "config"
        config_dir.mkdir(parents=True, exist_ok=True)
        (config_dir / "python_forkserver").write_text(content)

    def test_off_by_default(self) -> None:
        self.assertFalse(forkserver_setting.load(self.source_dir, {}))

    def test_config_file(self) -> None:
        for content, expected in (
            (f"{_ENV}=1\n", True),
            (f"{_ENV}=true\n", True),
            (f"export {_ENV}=yes\n", True),
            (f"# comment\n{_ENV}=0\n", False),
            (f"{_ENV}=invalid\n", False),
            ("UNRELATED=1\n", False),
        ):
            with self.subTest(content=content):
                self._write_config(content)
                self.assertEqual(
                    forkserver_setting.load(self.source_dir, {}), expected
                )

    def test_environment_overrides_config_file(self) -> None:
        self._write_config(f"{_ENV}=1\n")
        self.assertFalse(forkserver_setting.load(self.source_dir, {_ENV: "0"}))
        self.assertTrue(forkserver_setting.load(self.source_dir, {_ENV: ""}))
        self._write_config(f"{_ENV}=0\n")
        self.assertTrue(forkserver_setting.load(self.source_dir, {_ENV: "1"}))


if __name__ == "__main__":
    unittest.main()
