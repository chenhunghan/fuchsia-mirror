# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""A Mobly test that outlives its timeout, used to test python_mobly_test().

When it receives SIGTERM, Mobly aborts the test and runs `teardown_test`,
which takes longer than the 1s that mobly_driver gives leftover processes
before SIGKILL, then writes a marker file. The marker is only written if the
SIGTERM sent by mobly_driver at the start of its cleanup period reaches this
process directly, and the process then gets its cleanup period.
"""

import os
import time

from mobly import base_test, test_runner

# Longer than mobly_driver's `_LEFTOVER_PROCESS_GRACE_PERIOD` (1s).
_CLEANUP_DURATION_SECS = 2


class FakeTimeoutMoblyTest(base_test.BaseTestClass):
    """A Mobly test that does not need any device, and never finishes on its own."""

    def test_outlives_timeout(self):
        time.sleep(600)

    def teardown_test(self):
        time.sleep(_CLEANUP_DURATION_SECS)
        marker = os.path.join(os.environ["FUCHSIA_TEST_OUTDIR"], "cleanup_done")
        with open(marker, "w"):
            pass


if __name__ == "__main__":
    test_runner.main()
