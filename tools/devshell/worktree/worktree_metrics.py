# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

SNAPSHOT_INTERVAL_SEC: float = 24.0 * 60.0 * 60.0  # 24 hours


class WorktreeMetricsTracker:
    """Collects and aggregates worktree usage metrics locally throughout the day,

    uploading a time-weighted statistical summary every 24 hours via fx metrics.
    """

    def __init__(
        self,
        jiri_root: Path,
        fuchsia_dir: Path,
        metrics_file_path: Path | None = None,
        snapshot_interval_sec: float = SNAPSHOT_INTERVAL_SEC,
    ):
        self.jiri_root = Path(jiri_root).resolve()
        self.fuchsia_dir = Path(fuchsia_dir).resolve()
        self.metrics_file = (
            Path(metrics_file_path).resolve()
            if metrics_file_path is not None
            else self.fuchsia_dir / ".fx" / "metrics" / "worktree_metrics.json"
        )
        self.snapshot_interval_sec = snapshot_interval_sec

    def _load_or_init(
        self,
        now: float,
        current_total: int,
        current_leased: int,
        current_built_recently: int,
        current_not_built_recently: int,
    ) -> dict[str, Any]:
        if self.metrics_file.exists():
            try:
                content = json.loads(self.metrics_file.read_text())
                if (
                    "period_start_ts" in content
                    and "last_change_ts" in content
                    and "accumulated_total_seconds" in content
                    and "accumulated_leased_seconds" in content
                ):
                    content.setdefault(
                        "current_built_recently", current_built_recently
                    )
                    content.setdefault(
                        "current_not_built_recently", current_not_built_recently
                    )
                    content.setdefault(
                        "accumulated_built_recently_seconds", 0.0
                    )
                    content.setdefault(
                        "accumulated_not_built_recently_seconds", 0.0
                    )
                    content.setdefault(
                        "min_built_recently", current_built_recently
                    )
                    content.setdefault(
                        "max_built_recently", current_built_recently
                    )
                    content.setdefault(
                        "min_not_built_recently", current_not_built_recently
                    )
                    content.setdefault(
                        "max_not_built_recently", current_not_built_recently
                    )
                    return content
                print(
                    f"Warning: Corrupted metrics file {self.metrics_file}, resetting state.",
                    file=sys.stderr,
                )
            except Exception as e:
                print(
                    f"Warning: Failed to read metrics file {self.metrics_file} ({e}), resetting state.",
                    file=sys.stderr,
                )

        return {
            "period_start_ts": now,
            "last_change_ts": now,
            "current_total": current_total,
            "current_leased": current_leased,
            "current_built_recently": current_built_recently,
            "current_not_built_recently": current_not_built_recently,
            "accumulated_total_seconds": 0.0,
            "accumulated_leased_seconds": 0.0,
            "accumulated_built_recently_seconds": 0.0,
            "accumulated_not_built_recently_seconds": 0.0,
            "min_total": current_total,
            "max_total": current_total,
            "min_leased": current_leased,
            "max_leased": current_leased,
            "min_built_recently": current_built_recently,
            "max_built_recently": current_built_recently,
            "min_not_built_recently": current_not_built_recently,
            "max_not_built_recently": current_not_built_recently,
        }

    def record_state(
        self,
        current_total: int,
        current_leased: int,
        current_built_recently: int = 0,
        current_not_built_recently: int = 0,
        now: float | None = None,
    ) -> bool:
        """Records the latest worktree counts.

        Calculates time-weighted metrics and triggers a daily snapshot upload
        if the snapshot interval has elapsed.

        Returns True if a daily snapshot was emitted, False otherwise.
        """
        if now is None:
            now = time.time()

        data = self._load_or_init(
            now,
            current_total,
            current_leased,
            current_built_recently,
            current_not_built_recently,
        )
        emitted_snapshot = False

        # If period duration has exceeded the snapshot interval, finalize and upload
        if (now - data["period_start_ts"]) >= self.snapshot_interval_sec:
            self._finalize_and_upload(data, now)
            emitted_snapshot = True
            # Reset period for the new 24h window
            data = {
                "period_start_ts": now,
                "last_change_ts": now,
                "current_total": current_total,
                "current_leased": current_leased,
                "current_built_recently": current_built_recently,
                "current_not_built_recently": current_not_built_recently,
                "accumulated_total_seconds": 0.0,
                "accumulated_leased_seconds": 0.0,
                "accumulated_built_recently_seconds": 0.0,
                "accumulated_not_built_recently_seconds": 0.0,
                "min_total": current_total,
                "max_total": current_total,
                "min_leased": current_leased,
                "max_leased": current_leased,
                "min_built_recently": current_built_recently,
                "max_built_recently": current_built_recently,
                "min_not_built_recently": current_not_built_recently,
                "max_not_built_recently": current_not_built_recently,
            }
        else:
            dt = max(0.0, now - data["last_change_ts"])
            data["accumulated_total_seconds"] += data["current_total"] * dt
            data["accumulated_leased_seconds"] += data["current_leased"] * dt
            data["accumulated_built_recently_seconds"] += (
                data["current_built_recently"] * dt
            )
            data["accumulated_not_built_recently_seconds"] += (
                data["current_not_built_recently"] * dt
            )
            data["last_change_ts"] = now
            data["current_total"] = current_total
            data["current_leased"] = current_leased
            data["current_built_recently"] = current_built_recently
            data["current_not_built_recently"] = current_not_built_recently
            data["min_total"] = min(data["min_total"], current_total)
            data["max_total"] = max(data["max_total"], current_total)
            data["min_leased"] = min(data["min_leased"], current_leased)
            data["max_leased"] = max(data["max_leased"], current_leased)
            data["min_built_recently"] = min(
                data["min_built_recently"], current_built_recently
            )
            data["max_built_recently"] = max(
                data["max_built_recently"], current_built_recently
            )
            data["min_not_built_recently"] = min(
                data["min_not_built_recently"], current_not_built_recently
            )
            data["max_not_built_recently"] = max(
                data["max_not_built_recently"], current_not_built_recently
            )

        self._save(data)
        return emitted_snapshot

    def _compute_averages(
        self, data: dict[str, Any], now: float
    ) -> tuple[float, dict[str, float]]:
        dt = max(0.0, now - data["last_change_ts"])
        total_accum = data["accumulated_total_seconds"] + (
            data["current_total"] * dt
        )
        leased_accum = data["accumulated_leased_seconds"] + (
            data["current_leased"] * dt
        )
        built_recently_accum = data["accumulated_built_recently_seconds"] + (
            data["current_built_recently"] * dt
        )
        not_built_recently_accum = data[
            "accumulated_not_built_recently_seconds"
        ] + (data["current_not_built_recently"] * dt)
        duration = max(1.0, now - data["period_start_ts"])
        averages = {
            "total": round(total_accum / duration, 2),
            "leased": round(leased_accum / duration, 2),
            "built_recently": round(built_recently_accum / duration, 2),
            "not_built_recently": round(not_built_recently_accum / duration, 2),
        }
        return duration, averages

    def _finalize_and_upload(self, data: dict[str, Any], now: float) -> None:
        _, averages = self._compute_averages(data, now)

        label = json.dumps(
            {
                "at": averages["total"],
                "mnt": data["min_total"],
                "mxt": data["max_total"],
                "al": averages["leased"],
                "mnl": data["min_leased"],
                "mxl": data["max_leased"],
                "abr": averages["built_recently"],
                "mnbr": data["min_built_recently"],
                "mxbr": data["max_built_recently"],
                "anbr": averages["not_built_recently"],
                "mnnbr": data["min_not_built_recently"],
                "mxnbr": data["max_not_built_recently"],
            },
            separators=(",", ":"),
        )

        self._emit_telemetry("daily_snapshot", label)

    def _emit_telemetry(self, action: str, label: str) -> None:
        report_script = (
            self.fuchsia_dir / "tools/devshell/lib/metrics_custom_report.sh"
        )
        if not report_script.exists():
            raise FileNotFoundError(
                f"Metrics reporting script not found at {report_script}"
            )

        kwargs: dict[str, Any] = {
            "stdout": subprocess.DEVNULL,
            "stderr": subprocess.DEVNULL,
        }

        # If file descriptor 10 is open (from scripts/fx), preserve it across exec
        # so the telemetry event is bundled into the same GA4 session batch.
        try:
            os.fstat(10)
            kwargs["pass_fds"] = (10,)
        except OSError:
            pass

        subprocess.Popen(
            ["bash", str(report_script), "worktree", action, label],
            **kwargs,
        )

    def get_summary(
        self,
        current_total: int,
        current_leased: int,
        current_built_recently: int,
        current_not_built_recently: int,
        now: float | None = None,
    ) -> dict[str, Any]:
        if now is None:
            now = time.time()
        data = self._load_or_init(
            now,
            current_total,
            current_leased,
            current_built_recently,
            current_not_built_recently,
        )
        duration, averages = self._compute_averages(data, now)

        return {
            "period_start_ts": data["period_start_ts"],
            "period_duration_sec": duration,
            "current": {
                "total": current_total,
                "leased": current_leased,
                "built_recently": current_built_recently,
                "not_built_recently": current_not_built_recently,
            },
            "average": averages,
            "min": {
                "total": min(data["min_total"], current_total),
                "leased": min(data["min_leased"], current_leased),
                "built_recently": min(
                    data["min_built_recently"], current_built_recently
                ),
                "not_built_recently": min(
                    data["min_not_built_recently"], current_not_built_recently
                ),
            },
            "max": {
                "total": max(data["max_total"], current_total),
                "leased": max(data["max_leased"], current_leased),
                "built_recently": max(
                    data["max_built_recently"], current_built_recently
                ),
                "not_built_recently": max(
                    data["max_not_built_recently"], current_not_built_recently
                ),
            },
            "seconds_until_next_snapshot": max(
                0.0, self.snapshot_interval_sec - duration
            ),
        }

    def _save(self, data: dict[str, Any]) -> None:
        self.metrics_file.parent.mkdir(parents=True, exist_ok=True)
        temp_file = self.metrics_file.with_suffix(".tmp")
        temp_file.write_text(json.dumps(data, indent=2))
        temp_file.replace(self.metrics_file)
