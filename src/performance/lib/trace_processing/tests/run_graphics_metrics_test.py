#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Unit tests for run_graphics_metrics.py."""

import json
import os
import tempfile
import unittest

import test_utils
from reporting import metrics
from trace_processing import run_graphics_metrics, trace_utils
from trace_processing.metrics import scenic


class RunGraphicsMetricsTest(unittest.TestCase):
    """Tests for run_graphics_metrics.main()."""

    def test_main_writes_fuchsiaperf_json(self) -> None:
        """Runs ScenicMetricsProcessor on an FXT trace and writes fuchsiaperf JSON."""
        fxt_path = test_utils.runtime_dep_path("scenic_metric.fxt")
        expected = trace_utils.standard_metrics_set(
            values=[0.09, 0.08],
            label_prefix="RenderCpu",
            unit=metrics.Unit.milliseconds,
        ) + trace_utils.standard_metrics_set(
            values=[0.11, 0.112],
            label_prefix="RenderTotal",
            unit=metrics.Unit.milliseconds,
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            output_path = os.path.join(tmpdir, "out.fuchsiaperf.json")
            run_graphics_metrics.main([fxt_path, output_path])
            with open(output_path, encoding="utf-8") as f:
                payload = json.load(f)

        self.assertSequenceEqual(
            payload, [result.to_json("Manual") for result in expected]
        )

    def test_main_empty_scenic_trace_writes_empty_json(self) -> None:
        """Writes an empty JSON list when the trace has no scenic events."""
        fxt_path = test_utils.runtime_dep_path("cpu_metric.fxt")
        with tempfile.TemporaryDirectory() as tmpdir:
            output_path = os.path.join(tmpdir, "empty.fuchsiaperf.json")
            with self.assertLogs(scenic._LOGGER, level="WARNING"):
                run_graphics_metrics.main([fxt_path, output_path])
            with open(output_path, encoding="utf-8") as f:
                payload = json.load(f)

        self.assertSequenceEqual(payload, [])

    def test_main_missing_args_raises_system_exit(self) -> None:
        """Exits with usage error when required positional args are omitted."""
        with self.assertRaises(SystemExit) as ctx:
            run_graphics_metrics.main([])
        self.assertNotEqual(ctx.exception.code, 0)


if __name__ == "__main__":
    unittest.main()
