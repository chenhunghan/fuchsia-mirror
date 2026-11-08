# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import contextlib
import io
import tempfile
import unittest
import unittest.mock as mock

import build_dir


class TestBuildDir(unittest.TestCase):
    @mock.patch.dict("os.environ", clear=True)
    def test_missing(self) -> None:
        self.assertRaises(
            build_dir.GetBuildDirectoryError,
            lambda: build_dir.get_build_directory(),
        )

    @mock.patch.dict(
        "os.environ", {"FUCHSIA_BUILD_DIR_FROM_FX": "/tmp/out/foo"}
    )
    def test_present(self) -> None:
        self.assertEqual(
            "/tmp/out/foo", build_dir.get_build_directory().as_posix()
        )

    def test_get_main_pb_name(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with open(f"{tmp}/args.json", "w") as f:
                f.write("{}")
            self.assertIsNone(build_dir.get_main_pb_name(tmp))

            with open(f"{tmp}/args.json", "w") as f:
                f.write('{"main_pb_label": "//products:pb.a"}')
            self.assertEqual("pb.a", build_dir.get_main_pb_name(tmp))
            self.assertIsNone(
                build_dir.get_main_pb_name(tmp, fallback_to_label_name=False)
            )

            with open(f"{tmp}/product_bundles.json", "w") as f:
                f.write(
                    '[{"name": "a-extra", "label": "//products:pb.a_extra(//tc)"},'
                    ' {"name": "bundle.a", "label": "//products:pb.a(//tc)"}]'
                )
            self.assertEqual("bundle.a", build_dir.get_main_pb_name(tmp))
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(0, build_dir.main(["--build-dir", tmp]))
            self.assertEqual("bundle.a\n", out.getvalue())
