#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import generate_package_debug_symbols_manifest

# Maps blob file names to fake build IDs. Files not listed here are treated as
# non-ELF files.
_BUILD_IDS = {
    "foo_bin": "aa11111111",
    "bar_bin": "bb22222222",
    "libshared.so": "cc33333333",
    "nosymbols_bin": "dd44444444",
}


def _fake_extract_gnu_build_id(path: str) -> str:
    return _BUILD_IDS.get(os.path.basename(path), "")


class GeneratePackageDebugSymbolsManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        self._temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self._temp_dir.cleanup)
        self.root = Path(self._temp_dir.name)

        cwd = os.getcwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, cwd)

        patcher = mock.patch.object(
            generate_package_debug_symbols_manifest,
            "extract_gnu_build_id",
            _fake_extract_gnu_build_id,
        )
        patcher.start()
        self.addCleanup(patcher.stop)

        for name, build_id in _BUILD_IDS.items():
            if name == "nosymbols_bin":
                continue
            debug_file = (
                self.root / ".build-id" / build_id[:2] / f"{build_id[2:]}.debug"
            )
            debug_file.parent.mkdir(parents=True, exist_ok=True)
            debug_file.touch()

    def _write_package(self, name: str, blobs: dict[str, str]) -> str:
        """Writes a package manifest with file-relative blob paths.

        Args:
            name: The package name, also used as its directory.
            blobs: Maps package-relative dest paths to blob file names.

        Returns:
            The path of the manifest, relative to the working directory.
        """
        package_dir = self.root / name
        package_dir.mkdir()
        for blob_file in blobs.values():
            (package_dir / blob_file).touch()
        (package_dir / "meta.far").touch()
        manifest = {
            "version": "1",
            "package": {"name": name, "version": "0"},
            "blob_sources_relative": "file",
            "blobs": [
                {"source_path": "meta.far", "path": "meta/"},
            ]
            + [
                {"source_path": blob_file, "path": dest}
                for dest, blob_file in blobs.items()
            ],
        }
        manifest_path = f"{name}/package_manifest.json"
        (self.root / manifest_path).write_text(json.dumps(manifest))
        return manifest_path

    def _run(self, *args: str) -> int:
        with mock.patch.object(
            sys,
            "argv",
            [
                "generate_package_debug_symbols_manifest.py",
                "--package-label",
                "//alice:bob",
                "--debug-symbols-dir",
                ".",
                "--output-debug-manifest",
                "debug.json",
                "--output-build-ids-txt",
                "build_ids.txt",
                "--depfile",
                "debug.d",
                *args,
            ],
        ):
            return generate_package_debug_symbols_manifest.main()

    def _debug_manifest(self) -> list[dict[str, str]]:
        return json.loads((self.root / "debug.json").read_text())

    def test_single_package_manifest(self) -> None:
        manifest = self._write_package(
            "foo",
            {
                "bin/foo": "foo_bin",
                "bin/nosymbols": "nosymbols_bin",
                "data/text": "text_file",
            },
        )

        self.assertEqual(self._run("--package-manifest", manifest), 0)

        self.assertEqual(
            self._debug_manifest(),
            [
                {
                    "cpu": "x64",
                    "debug": ".build-id/aa/11111111.debug",
                    "dest_path": "bin/foo",
                    "elf_build_id": "aa11111111",
                    "label": "//alice:bob",
                    "os": "fuchsia",
                }
            ],
        )
        self.assertEqual(
            (self.root / "build_ids.txt").read_text(),
            "aa11111111 foo/foo_bin\ndd44444444 foo/nosymbols_bin\n",
        )
        depfile = (self.root / "debug.d").read_text()
        self.assertIn("foo/foo_bin", depfile)
        self.assertNotIn(manifest, depfile)

    def test_package_manifests_list(self) -> None:
        foo = self._write_package(
            "foo", {"bin/foo": "foo_bin", "lib/libshared.so": "libshared.so"}
        )
        bar = self._write_package(
            "bar", {"bin/bar": "bar_bin", "lib/libshared.so": "libshared.so"}
        )
        (self.root / "packages.list").write_text(
            json.dumps({"content": {"manifests": [foo, bar]}, "version": "1"})
        )

        self.assertEqual(
            self._run("--package-manifests-list", "packages.list"), 0
        )

        # The library shared by both packages is only listed once.
        self.assertEqual(
            [
                (entry["dest_path"], entry["elf_build_id"])
                for entry in self._debug_manifest()
            ],
            [
                ("bin/foo", "aa11111111"),
                ("lib/libshared.so", "cc33333333"),
                ("bin/bar", "bb22222222"),
            ],
        )
        # The shared library is attributed to the first package in both
        # outputs.
        self.assertEqual(
            (self.root / "build_ids.txt").read_text(),
            "aa11111111 foo/foo_bin\n"
            "cc33333333 foo/libshared.so\n"
            "bb22222222 bar/bar_bin\n",
        )
        depfile = (self.root / "debug.d").read_text()
        for path in ["packages.list", foo, bar, "bar/bar_bin"]:
            self.assertIn(path, depfile)

    def test_check_missing(self) -> None:
        manifest = self._write_package(
            "foo", {"bin/nosymbols": "nosymbols_bin"}
        )

        self.assertEqual(
            self._run("--package-manifest", manifest, "--check-missing"), 1
        )


if __name__ == "__main__":
    unittest.main()
