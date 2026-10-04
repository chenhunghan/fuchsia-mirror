# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
from typing import Any

from worktree import WorktreeState
from worktree_metrics import WorktreeMetricsTracker
from worktree_pool import WorktreePool


def _format_duration(seconds: float) -> str:
    total_sec = int(seconds)
    hours = total_sec // 3600
    minutes = (total_sec % 3600) // 60
    if hours > 0:
        return f"{hours}h {minutes}m"
    return f"{minutes}m {total_sec % 60}s"


def run(args: Any, pool: WorktreePool) -> None:
    worktrees = pool.get_worktrees()
    total_count = len(worktrees)
    leased_count = sum(
        1 for wt in worktrees if wt.get_state() == WorktreeState.LEASED
    )
    built_recently_count = sum(
        1 for wt in worktrees if wt.is_built_recently(max_age_days=7)
    )
    not_built_recently_count = total_count - built_recently_count

    tracker = WorktreeMetricsTracker(pool.jiri_root, pool.fuchsia_dir)
    summary = tracker.get_summary(
        current_total=total_count,
        current_leased=leased_count,
        current_built_recently=built_recently_count,
        current_not_built_recently=not_built_recently_count,
    )

    if getattr(args, "json", False):
        print(json.dumps(summary, indent=2))
        return

    dur_str = _format_duration(summary["period_duration_sec"])
    next_snap_str = _format_duration(summary["seconds_until_next_snapshot"])

    print("=== fx worktree metrics ===")
    print(f"Measurement Window: {dur_str} (24h rolling period)")
    print(f"Next Snapshot Upload: in {next_snap_str}\n")
    print(
        f"{'Category':<18} {'Current':<9} {'Avg (Time-Weighted)':<21} {'Min':<6} {'Peak (Max)':<10}"
    )
    print("-" * 68)

    category_labels = (
        ("total", "Total"),
        ("leased", "Leased"),
        ("built_recently", "BuiltRecently"),
        ("not_built_recently", "NotBuiltRecently"),
    )
    for cat, label in category_labels:
        cur = summary["current"][cat]
        avg = summary["average"][cat]
        mn = summary["min"][cat]
        mx = summary["max"][cat]
        print(f"{label:<18} {cur:<9} {avg:<21.2f} {mn:<6} {mx:<10}")
