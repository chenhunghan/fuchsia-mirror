#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Mobly test for Serial transport."""

import logging
import time

import fuchsia_base_test
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class SerialTransportTests(fuchsia_base_test.FuchsiaBaseTest):
    """Serial transport tests"""

    async def test_send(self) -> None:
        """Test case for Serial.send()"""
        for cmd in [
            "echo hi",
            "echo hello",
            "echo foo",
            "echo bar",
            "ls",
            "ls -l",
        ]:
            self.dut.serial.send(
                cmd=cmd,
            )

    async def test_read(self) -> None:
        """
        Test case for Serial.read()

        Only verifies that data can be read from the
        serial stream.
        """
        read_end_time = time.time() + 10
        string_found = False
        while time.time() < read_end_time:
            try:
                read_data = self.dut.serial.read()
                if len(read_data) > 0:
                    string_found = True
            except Exception:
                time.sleep(0.1)

        asserts.assert_true(
            string_found,
            f"Data not read within 10 seconds.",
        )

    async def test_send_and_recv(self) -> None:
        """Test case for Serial.send_and_recv()."""
        output = self.dut.serial.send_and_recv(
            cmd='echo "foo""bar"',
            stop_tokens=("foobar",),
        )
        asserts.assert_in(
            "foobar",
            output,
            f"Expected 'foobar' in output, got: {output!r}",
        )


if __name__ == "__main__":
    test_runner.main()
