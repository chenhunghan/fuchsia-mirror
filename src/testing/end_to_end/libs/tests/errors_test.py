# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Tests for libs.errors."""

import unittest

from libs import errors, validation
from libs.proc import runner
from libs.ssh import provider


class TestErrorTest(unittest.TestCase):
    def test_details_only(self) -> None:
        e = errors.TestError("boom")
        self.assertEqual(e.details, "boom")
        self.assertIsNone(e.extras)
        self.assertEqual(str(e), "boom")

    def test_empty_extras(self) -> None:
        e = errors.TestError("boom", extras={})
        self.assertEqual(e.extras, {})
        self.assertEqual(str(e), "boom (extras: {})")

    def test_non_json_serializable_extras(self) -> None:
        # Mobly's TestError rejected non-JSON-serializable extras.
        extras = {"stderr": b"RTNETLINK answers: File exists", "returncode": 2}
        e = errors.TestError("Route already exists", extras=extras)
        self.assertIs(e.extras, extras)
        self.assertEqual(
            str(e),
            "Route already exists (extras: "
            "{'stderr': b'RTNETLINK answers: File exists', 'returncode': 2})",
        )


class SubclassTest(unittest.TestCase):
    def test_calledprocesstransporterror(self) -> None:
        e = runner.CalledProcessTransportError("Ssh timed out")
        self.assertIsInstance(e, errors.TestError)
        self.assertEqual(str(e), "Ssh timed out")

    def test_validatorerror(self) -> None:
        with self.assertRaises(errors.TestError) as cm:
            validation.MapValidator({}).get(str, "name")
        self.assertIsInstance(cm.exception, validation.FieldNotFoundError)
        self.assertIsNone(cm.exception.extras)

    def test_ssherror(self) -> None:
        result = runner.CalledProcessError(
            1, ["ls"], output=b"out", stderr=b"err"
        )
        e = provider.SSHError(["ls", "/tmp"], result, elapsed_sec=1.5)
        self.assertIsInstance(e, errors.TestError)
        self.assertIs(e.result, result)
        self.assertIn('SSH command "ls /tmp" unexpectedly returned 1', str(e))
        self.assertIn("stderr: err", str(e))
        self.assertIn("stdout: out", str(e))


if __name__ == "__main__":
    unittest.main()
