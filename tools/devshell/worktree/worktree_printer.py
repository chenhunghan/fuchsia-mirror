# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
from dataclasses import asdict, dataclass
from typing import Callable, Iterable

from utils import Colors, colorize
from worktree import Worktree


@dataclass
class SyncPrintData:
    behind: int
    new: int


@dataclass
class BuildDirPrintData:
    path: str
    active: bool
    config: str
    last_build: str
    last_build_sec: float | None = None


@dataclass
class WorktreePrintData:
    name: str
    task: str | None
    sync: SyncPrintData
    build_dirs: list[BuildDirPrintData]


def format_time_ago_plain(sec: float | None) -> str:
    if sec is None:
        return "never built"
    if sec < 60:
        return "just now"
    minutes = int(sec) // 60
    hours = minutes // 60
    days = hours // 24
    if days > 0:
        return f"{days}d ago"
    if hours > 0:
        return f"{hours}h ago"
    return f"{minutes}m ago"


def format_time_ago(sec: float | None) -> str:
    if sec is None:
        return colorize("never built", Colors.RED)
    return format_time_ago_plain(sec)


class WorktreePrinter:
    """Formats and displays Worktree information in terminal and JSON outputs.

    Provides helper methods to render worktree status, lease details, build
    timestamps, and sync health across list and pool subcommands.
    """

    @staticmethod
    def print_worktrees(
        worktrees: Iterable[Worktree],
        title_fn: Callable[[Worktree], str] | None = None,
    ) -> None:
        wt_list = list(worktrees)
        if not wt_list:
            return

        max_left_len = 0
        wt_entries = []

        for wt in wt_list:
            display_name = title_fn(wt) if title_fn else wt.name
            data = WorktreePrinter.format_worktree_data(wt)

            parts = []
            if data.sync.behind > 0:
                parts.append(colorize(f"{data.sync.behind} behind", Colors.RED))
            if data.sync.new > 0:
                parts.append(colorize(f"{data.sync.new} new", Colors.BLUE))

            build_entries = []
            for bd in data.build_dirs:
                suffix = " *" if bd.active else ""
                label = f"{bd.path}{suffix}:"
                max_left_len = max(max_left_len, 4 + len(label))
                time_str = format_time_ago(bd.last_build_sec)
                build_entries.append((label, bd.config, time_str, bd.active))

            wt_entries.append((display_name, parts, build_entries))

        wt_entries.sort(key=lambda item: item[0])
        left_width = max_left_len + 4

        for name, parts, builds in wt_entries:
            header = colorize(name, Colors.BOLD)
            if parts:
                header = f"{header} ({', '.join(parts)})"
            print(header)
            for i, (label, cfg, time_str, is_active) in enumerate(builds):
                is_last = i == len(builds) - 1
                prefix = "└── " if is_last else "├── "
                left_part = f"{prefix}{label}"
                line = f"{left_part:<{left_width}}{cfg} ({time_str})"
                if is_active:
                    line = colorize(line, Colors.GREEN)
                print(line)
            if builds:
                print()

    @staticmethod
    def format_worktree_data(
        wt: Worktree, include_inactive: bool = False
    ) -> WorktreePrintData:
        """Collects worktree status, build configs, and lease details into
        WorktreePrintData.
        """
        lease = wt.get_lease_info()
        task_name = lease.task_id if lease and lease.task_id else None

        if include_inactive:
            display_name = wt.name
        else:
            display_name = task_name if task_name else wt.name

        _, behind, new = wt.get_sync_status()

        try:
            selected_dir = wt.selected_build_dir()
        except (FileNotFoundError, ValueError, OSError):
            selected_dir = None

        build_entries: list[BuildDirPrintData] = []
        for bd in wt.build_dirs():
            try:
                out_rel = str(bd.path.relative_to(wt.path))
            except ValueError:
                out_rel = str(bd.path)
            is_active = selected_dir is not None and bd.path == selected_dir
            cfg = bd.get_build_config()
            time_ago_sec = bd.get_build_time_ago_sec()
            build_entries.append(
                BuildDirPrintData(
                    path=out_rel,
                    active=is_active,
                    config=cfg,
                    last_build=format_time_ago_plain(time_ago_sec),
                    last_build_sec=time_ago_sec,
                )
            )

        return WorktreePrintData(
            name=display_name,
            task=task_name,
            sync=SyncPrintData(behind=behind, new=new),
            build_dirs=build_entries,
        )

    @staticmethod
    def print_worktrees_json(
        worktrees: Iterable[Worktree], include_inactive: bool = False
    ) -> None:
        """Serializes and outputs worktree entries as JSON to stdout."""
        entries = []
        for wt in worktrees:
            d = asdict(
                WorktreePrinter.format_worktree_data(
                    wt, include_inactive=include_inactive
                )
            )
            for bd in d["build_dirs"]:
                bd.pop("last_build_sec", None)
            entries.append(d)
        print(json.dumps(entries))
