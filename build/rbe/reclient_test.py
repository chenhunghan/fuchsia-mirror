#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for build.rbe.reclient."""

import tempfile
import unittest
from pathlib import Path

from build.rbe import reclient


class ReclientFlagsTests(unittest.TestCase):
    def test_cfg_path_to_flags(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            cfg = Path(td) / "test.cfg"
            cfg.write_text("# comment\nfoo=bar\nbaz=qux\n")
            self.assertEqual(
                reclient.cfg_path_to_flags(cfg),
                ["--foo=bar", "--baz=qux"],
            )

    def test_gcert_auth_flags(self) -> None:
        flags = reclient.gcert_auth_flags()
        self.assertIn("--use_application_default_credentials=false", flags)
        self.assertIn("--credentials_helper=execrel://", flags)
        self.assertIn(
            "--credentials_helper_args=--auth_source=automaticAuth --gcert_refresh_timeout=10",
            flags,
        )
        self.assertIn("--use_external_auth_token=true", flags)

    def test_gce_auth_flags(self) -> None:
        self.assertEqual(
            reclient.gce_auth_flags(),
            ["--use_gce_credentials=true"],
        )

    def test_adc_auth_flags(self) -> None:
        self.assertEqual(
            reclient.adc_auth_flags(),
            ["--use_application_default_credentials=true"],
        )


class CanonicalWorkingDirTests(unittest.TestCase):
    def test_empty(self) -> None:
        self.assertEqual(reclient.canonical_working_dir(Path("")), Path(""))

    def test_one_level(self) -> None:
        self.assertEqual(
            reclient.canonical_working_dir(Path("build-here")),
            Path("set_by_reclient"),
        )

    def test_two_levels(self) -> None:
        self.assertEqual(
            reclient.canonical_working_dir(Path("build/there")),
            Path("set_by_reclient/a"),
        )

    def test_three_levels(self) -> None:
        self.assertEqual(
            reclient.canonical_working_dir(Path("build/inside/there")),
            Path("set_by_reclient/a/a"),
        )


class CanonicalPathNormalizationTests(unittest.TestCase):
    def test_is_canonical_working_dir_path(self) -> None:
        self.assertTrue(
            reclient.is_canonical_working_dir_path(
                Path("set_by_reclient/a/foo.h")
            )
        )
        self.assertFalse(
            reclient.is_canonical_working_dir_path(Path("src/foo.h"))
        )
        self.assertFalse(reclient.is_canonical_working_dir_path(Path("")))

    def test_normalized_exec_root_relpath_dynamic_sandbox_prefix(self) -> None:
        build_subdir = Path("out/default")
        self.assertEqual(
            reclient.normalized_exec_root_relpath(
                Path("/worker/sandbox/123/set_by_reclient/a/gen/foo.h"),
                build_subdir,
            ),
            Path("set_by_reclient/a/gen/foo.h"),
        )
        self.assertEqual(
            reclient.normalized_exec_root_relpath(
                Path(
                    "/worker/sandbox/set_by_reclient/a/../../tools/fidl/askama.toml"
                ),
                build_subdir,
            ),
            Path("tools/fidl/askama.toml"),
        )

    def test_normalized_exec_root_relpath_nested(self) -> None:
        build_subdir = Path("out/default")
        self.assertEqual(
            reclient.normalized_exec_root_relpath(
                Path(
                    "/worker/set_by_reclient/nested/set_by_reclient/a/gen/foo.h"
                ),
                build_subdir,
            ),
            Path("set_by_reclient/a/gen/foo.h"),
        )
        # Absolute path with a nested literal set_by_reclient subdir under the
        # build dir anchors at the outer canonical working dir.
        self.assertEqual(
            reclient.normalized_exec_root_relpath(
                Path(
                    "/worker/sandbox/123/set_by_reclient/a/obj/set_by_reclient/foo.h"
                ),
                build_subdir,
            ),
            Path("set_by_reclient/a/obj/set_by_reclient/foo.h"),
        )
        # Workspace path with a literal set_by_reclient directory lacking `a`
        # components is not matched as a canonical working directory.
        self.assertIsNone(
            reclient.normalized_exec_root_relpath(
                Path("/b/f/w/src/set_by_reclient/foo.h"),
                build_subdir,
            )
        )
        # Relative path with a nested literal set_by_reclient subdir anchors at
        # the outer canonical working dir rather than the inner subdir.
        self.assertEqual(
            reclient.normalized_exec_root_relpath(
                Path("set_by_reclient/a/obj/set_by_reclient/foo.h"),
                build_subdir,
            ),
            Path("set_by_reclient/a/obj/set_by_reclient/foo.h"),
        )

    def test_normalized_exec_root_relpath_relative(self) -> None:
        build_subdir = Path("out/default")
        self.assertEqual(
            reclient.normalized_exec_root_relpath(
                Path("set_by_reclient/a/gen/foo.h"),
                build_subdir,
            ),
            Path("set_by_reclient/a/gen/foo.h"),
        )
        self.assertEqual(
            reclient.normalized_exec_root_relpath(
                Path("../../set_by_reclient/a/gen/foo.h"),
                build_subdir,
            ),
            Path("set_by_reclient/a/gen/foo.h"),
        )
        # Relative literal subdir named set_by_reclient (lacking `a` depth) is
        # left untouched.
        self.assertIsNone(
            reclient.normalized_exec_root_relpath(
                Path("set_by_reclient/foo.h"),
                build_subdir,
            )
        )
        self.assertIsNone(
            reclient.normalized_exec_root_relpath(
                Path("obj/set_by_reclient/a/foo.h"),
                build_subdir,
            )
        )
        self.assertIsNone(
            reclient.normalized_exec_root_relpath(
                Path("src/foo.h"),
                build_subdir,
            )
        )

    def test_normalized_exec_root_relpath_escapes_exec_root(self) -> None:
        build_subdir = Path("out/default")
        self.assertIsNone(
            reclient.normalized_exec_root_relpath(
                Path(
                    "/worker/sandbox/set_by_reclient/a/../../../usr/include/stdio.h"
                ),
                build_subdir,
            )
        )

    def test_relativize_canonical_path(self) -> None:
        build_subdir = Path("out/default")
        self.assertEqual(
            reclient.relativize_canonical_path(
                Path("/worker/sandbox/123/set_by_reclient/a/gen/foo.h"),
                build_subdir,
            ),
            Path("gen/foo.h"),
        )
        self.assertEqual(
            reclient.relativize_canonical_path(
                Path(
                    "/worker/sandbox/123/set_by_reclient/a/obj/set_by_reclient/foo.h"
                ),
                build_subdir,
            ),
            Path("obj/set_by_reclient/foo.h"),
        )
        self.assertEqual(
            reclient.relativize_canonical_path(
                Path(
                    "/worker/sandbox/set_by_reclient/a/../../tools/fidl/askama.toml"
                ),
                build_subdir,
            ),
            Path("../../tools/fidl/askama.toml"),
        )
        self.assertEqual(
            reclient.relativize_canonical_path(
                Path("../../set_by_reclient/a/gen/foo.h"),
                build_subdir,
            ),
            Path("gen/foo.h"),
        )
        self.assertEqual(
            reclient.relativize_canonical_path(
                Path("set_by_reclient/a/obj/set_by_reclient/foo.h"),
                build_subdir,
            ),
            Path("obj/set_by_reclient/foo.h"),
        )
        # Workspace or build literal `set_by_reclient` directory lacking `a`
        # depth is not mis-anchored.
        self.assertIsNone(
            reclient.relativize_canonical_path(
                Path("/b/f/w/src/set_by_reclient/foo.h"),
                build_subdir,
            )
        )
        self.assertIsNone(
            reclient.relativize_canonical_path(
                Path("set_by_reclient/foo.h"),
                build_subdir,
            )
        )
        # Mismatched leading '..' depth (1 or 3 instead of 2) is rejected.
        self.assertIsNone(
            reclient.relativize_canonical_path(
                Path("../set_by_reclient/a/gen/foo.h"),
                build_subdir,
            )
        )
        self.assertIsNone(
            reclient.relativize_canonical_path(
                Path("../../../set_by_reclient/a/gen/foo.h"),
                build_subdir,
            )
        )
        # Empty build_subdir has no canonical set_by_reclient directory.
        self.assertIsNone(
            reclient.relativize_canonical_path(
                Path("set_by_reclient/gen/foo.h"),
                Path(""),
            )
        )
        self.assertIsNone(
            reclient.relativize_canonical_path(
                Path("obj/set_by_reclient/a/foo.h"),
                build_subdir,
            )
        )


if __name__ == "__main__":
    unittest.main()
