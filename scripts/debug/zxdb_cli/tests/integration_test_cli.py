# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from tests.test_attach_detach import TestAttachDetachIntegration
from tests.test_evaluate_and_console import TestEvaluateAndConsoleIntegration
from tests.test_events import TestEventsIntegration
from tests.test_execution import TestExecutionIntegration
from tests.test_lifecycle import TestDaemonLifecycleIntegration
from tests.test_stack_trace import TestStackTraceIntegration

__all__ = [
    "TestAttachDetachIntegration",
    "TestDaemonLifecycleIntegration",
    "TestEvaluateAndConsoleIntegration",
    "TestEventsIntegration",
    "TestExecutionIntegration",
    "TestStackTraceIntegration",
    "unittest",
]

if __name__ == "__main__":
    unittest.main()
