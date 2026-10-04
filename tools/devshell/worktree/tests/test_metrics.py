# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

worktree_dir = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, worktree_dir)

from worktree import Worktree
from worktree_metrics import WorktreeMetricsTracker


class TestWorktreeMetricsTracker(unittest.TestCase):
    def setUp(self) -> None:
        self.temp_dir = Path(tempfile.mkdtemp())
        self.jiri_root = self.temp_dir / ".jiri_root"
        self.jiri_root.mkdir(parents=True, exist_ok=True)
        self.fuchsia_dir = self.temp_dir
        self.metrics_file = (
            self.fuchsia_dir / ".fx" / "metrics" / "worktree_metrics.json"
        )

    def tearDown(self) -> None:
        shutil.rmtree(self.temp_dir)

    def test_initial_record_state(self) -> None:
        tracker = WorktreeMetricsTracker(
            self.jiri_root,
            self.fuchsia_dir,
        )
        t0 = 1000.0
        emitted = tracker.record_state(
            current_total=4,
            current_leased=2,
            current_built_recently=3,
            current_not_built_recently=1,
            now=t0,
        )
        self.assertFalse(emitted)
        self.assertTrue(self.metrics_file.exists())

        data = json.loads(self.metrics_file.read_text())
        self.assertEqual(data["period_start_ts"], 1000.0)
        self.assertEqual(data["last_change_ts"], 1000.0)
        self.assertEqual(data["current_total"], 4)
        self.assertEqual(data["current_leased"], 2)
        self.assertEqual(data["current_built_recently"], 3)
        self.assertEqual(data["current_not_built_recently"], 1)
        self.assertEqual(data["accumulated_total_seconds"], 0.0)
        self.assertEqual(data["accumulated_leased_seconds"], 0.0)
        self.assertEqual(data["accumulated_built_recently_seconds"], 0.0)
        self.assertEqual(data["accumulated_not_built_recently_seconds"], 0.0)
        self.assertEqual(data["min_total"], 4)
        self.assertEqual(data["max_total"], 4)
        self.assertEqual(data["min_leased"], 2)
        self.assertEqual(data["max_leased"], 2)
        self.assertEqual(data["min_built_recently"], 3)
        self.assertEqual(data["max_built_recently"], 3)
        self.assertEqual(data["min_not_built_recently"], 1)
        self.assertEqual(data["max_not_built_recently"], 1)

    def test_time_accumulation_and_extrema(self) -> None:
        tracker = WorktreeMetricsTracker(
            self.jiri_root,
            self.fuchsia_dir,
            metrics_file_path=self.metrics_file,
        )
        t0 = 1000.0
        tracker.record_state(
            current_total=3,
            current_leased=1,
            current_built_recently=2,
            current_not_built_recently=1,
            now=t0,
        )

        # 1 hour later: add a worktree and build it (total: 4, leased: 2, built_recently: 3, not_built_recently: 1)
        t1 = 1000.0 + 3600.0
        emitted = tracker.record_state(
            current_total=4,
            current_leased=2,
            current_built_recently=3,
            current_not_built_recently=1,
            now=t1,
        )
        self.assertFalse(emitted)

        data = json.loads(self.metrics_file.read_text())
        self.assertEqual(data["accumulated_total_seconds"], 3 * 3600.0)
        self.assertEqual(data["accumulated_leased_seconds"], 1 * 3600.0)
        self.assertEqual(data["accumulated_built_recently_seconds"], 2 * 3600.0)
        self.assertEqual(
            data["accumulated_not_built_recently_seconds"], 1 * 3600.0
        )
        self.assertEqual(data["min_total"], 3)
        self.assertEqual(data["max_total"], 4)
        self.assertEqual(data["min_leased"], 1)
        self.assertEqual(data["max_leased"], 2)
        self.assertEqual(data["min_built_recently"], 2)
        self.assertEqual(data["max_built_recently"], 3)
        self.assertEqual(data["min_not_built_recently"], 1)
        self.assertEqual(data["max_not_built_recently"], 1)

    def test_daily_snapshot_trigger_and_rotation(self) -> None:
        tracker = WorktreeMetricsTracker(
            self.jiri_root,
            self.fuchsia_dir,
            metrics_file_path=self.metrics_file,
            snapshot_interval_sec=86400.0,
        )
        t0 = 1000.0
        tracker.record_state(
            current_total=2,
            current_leased=1,
            current_built_recently=1,
            current_not_built_recently=1,
            now=t0,
        )

        # 12 hours later: transition to total=4, leased=3, built_recently=3, not_built_recently=1
        t1 = t0 + 43200.0
        tracker.record_state(
            current_total=4,
            current_leased=3,
            current_built_recently=3,
            current_not_built_recently=1,
            now=t1,
        )

        # 24 hours from t0: trigger snapshot
        t2 = t0 + 86400.0
        with patch.object(tracker, "_emit_telemetry") as mock_emit:
            emitted = tracker.record_state(
                current_total=4,
                current_leased=3,
                current_built_recently=3,
                current_not_built_recently=1,
                now=t2,
            )
            self.assertTrue(emitted)
            mock_emit.assert_called_once_with(
                "daily_snapshot",
                '{"at":3.0,"mnt":2,"mxt":4,"al":2.0,"mnl":1,"mxl":3,'
                '"abr":2.0,"mnbr":1,"mxbr":3,"anbr":1.0,"mnnbr":1,"mxnbr":1}',
            )

        # Confirm data rotated for new period
        data = json.loads(self.metrics_file.read_text())
        self.assertEqual(data["period_start_ts"], t2)
        self.assertEqual(data["last_change_ts"], t2)
        self.assertEqual(data["accumulated_total_seconds"], 0.0)
        self.assertEqual(data["accumulated_leased_seconds"], 0.0)
        self.assertEqual(data["accumulated_built_recently_seconds"], 0.0)
        self.assertEqual(data["accumulated_not_built_recently_seconds"], 0.0)
        self.assertEqual(data["min_total"], 4)
        self.assertEqual(data["max_total"], 4)
        self.assertEqual(data["min_leased"], 3)
        self.assertEqual(data["max_leased"], 3)
        self.assertEqual(data["min_built_recently"], 3)
        self.assertEqual(data["max_built_recently"], 3)
        self.assertEqual(data["min_not_built_recently"], 1)
        self.assertEqual(data["max_not_built_recently"], 1)

    def test_missing_report_script_raises_loud_error(self) -> None:
        tracker = WorktreeMetricsTracker(
            self.jiri_root,
            self.fuchsia_dir,
            metrics_file_path=self.metrics_file,
        )
        with self.assertRaises(FileNotFoundError) as ctx:
            tracker._emit_telemetry("daily_snapshot", "label")
        self.assertIn("Metrics reporting script not found", str(ctx.exception))

    def test_emit_telemetry_runs_subprocess(self) -> None:
        report_script = (
            self.fuchsia_dir / "tools/devshell/lib/metrics_custom_report.sh"
        )
        report_script.parent.mkdir(parents=True, exist_ok=True)
        report_script.write_text("#!/bin/bash\nexit 0\n")

        tracker = WorktreeMetricsTracker(
            self.jiri_root,
            self.fuchsia_dir,
            metrics_file_path=self.metrics_file,
        )
        with patch("subprocess.Popen") as mock_popen:
            tracker._emit_telemetry("daily_snapshot", "test_label")
            mock_popen.assert_called_once()
            args, _ = mock_popen.call_args
            self.assertEqual(
                args[0],
                [
                    "bash",
                    str(report_script),
                    "worktree",
                    "daily_snapshot",
                    "test_label",
                ],
            )

    def test_corrupted_file_recovery(self) -> None:
        self.metrics_file.parent.mkdir(parents=True, exist_ok=True)
        self.metrics_file.write_text("invalid json content")
        tracker = WorktreeMetricsTracker(
            self.jiri_root,
            self.fuchsia_dir,
            metrics_file_path=self.metrics_file,
        )
        # Should not crash but reset to initial state
        emitted = tracker.record_state(
            current_total=3,
            current_leased=1,
            current_built_recently=2,
            current_not_built_recently=1,
            now=1000.0,
        )
        self.assertFalse(emitted)
        data = json.loads(self.metrics_file.read_text())
        self.assertEqual(data["current_total"], 3)
        self.assertEqual(data["current_built_recently"], 2)
        self.assertEqual(data["current_not_built_recently"], 1)

    def test_worktree_is_built_recently(self) -> None:
        wt_path = self.temp_dir / "my_wt"
        wt_path.mkdir(parents=True, exist_ok=True)
        wt = Worktree("my_wt", wt_path, self.fuchsia_dir)

        now = 1000000.0
        # No build directory -> not built recently (False)
        self.assertFalse(wt.is_built_recently(max_age_days=7, now=now))

        # Add build dir with recent .ninja_log (2 days old)
        build_dir = wt_path / "out" / "default_build"
        build_dir.mkdir(parents=True, exist_ok=True)
        (build_dir / "args.gn").write_text('build_info_board = "x64"\n')
        ninja_log = build_dir / ".ninja_log"
        ninja_log.write_text("# ninja log v5\n")
        mtime_recent = now - (2 * 86400)
        import os

        os.utime(ninja_log, (mtime_recent, mtime_recent))

        self.assertTrue(wt.is_built_recently(max_age_days=7, now=now))

        # Update .ninja_log to be 10 days old (> 7 days) -> not built recently (False)
        mtime_old = now - (10 * 86400)
        os.utime(ninja_log, (mtime_old, mtime_old))
        self.assertFalse(wt.is_built_recently(max_age_days=7, now=now))

    def test_get_summary(self) -> None:
        tracker = WorktreeMetricsTracker(
            self.jiri_root,
            self.fuchsia_dir,
            metrics_file_path=self.metrics_file,
            snapshot_interval_sec=86400.0,
        )
        t0 = 1000.0
        tracker.record_state(
            current_total=2,
            current_leased=1,
            current_built_recently=1,
            current_not_built_recently=1,
            now=t0,
        )

        t1 = t0 + 3600.0  # 1 hour later
        summary = tracker.get_summary(
            current_total=4,
            current_leased=2,
            current_built_recently=3,
            current_not_built_recently=1,
            now=t1,
        )

        self.assertEqual(summary["current"]["total"], 4)
        self.assertEqual(summary["current"]["leased"], 2)
        self.assertEqual(summary["current"]["built_recently"], 3)
        self.assertEqual(summary["current"]["not_built_recently"], 1)
        self.assertEqual(summary["average"]["total"], 2.0)
        self.assertEqual(summary["average"]["leased"], 1.0)
        self.assertEqual(summary["min"]["total"], 2)
        self.assertEqual(summary["max"]["total"], 4)
        self.assertEqual(
            summary["seconds_until_next_snapshot"], 86400.0 - 3600.0
        )


if __name__ == "__main__":
    unittest.main()
