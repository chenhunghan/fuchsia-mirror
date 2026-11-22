# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""Counters of the Python forkserver.

Each forker (forker.py) counts the workers it reaps and writes them to
`<stats>.<pid>` when it stops; the controller (server.py) merges those files
into `<stats>`.
"""

import dataclasses
import json
import os
from collections.abc import Mapping
from pathlib import Path
from typing import Any, NamedTuple


@dataclasses.dataclass
class Counters:
    requests: int = 0
    completed: int = 0
    exit_nonzero: int = 0
    signaled: int = 0
    worker_utime_s: float = 0.0
    worker_stime_s: float = 0.0

    def add(self, other: Mapping[str, Any]) -> None:
        """Adds the same-named counters in `other` (e.g. a forker's stats)."""
        for field in dataclasses.fields(self):
            value = getattr(self, field.name) + other[field.name]
            setattr(self, field.name, value)


class ForkerUsage(NamedTuple):
    """The forkers' own CPU time, summed over all of them.

    Each forker writes its CPU time as `server_{utime,stime}_s` (it is the
    server from its own point of view); the controller reports the sum under
    these names, next to its own `server_*_s`.
    """

    forkers_utime_s: float
    forkers_stime_s: float


class Metrics:
    """Counters, per forker and merged.

    Each forker writes its own to `<path>.<pid>`; the controller merges them
    into `<path>`.
    """

    def __init__(self, stats_path: str):
        self.stats_path = stats_path
        self.counters = Counters()

    def worker_done(self, status: int, ru: Any) -> None:
        """Records a reaped worker."""
        c = self.counters
        c.completed += 1
        c.worker_utime_s += ru.ru_utime
        c.worker_stime_s += ru.ru_stime
        code = os.waitstatus_to_exitcode(status)
        if code < 0:
            c.signaled += 1
        elif code != 0:
            c.exit_nonzero += 1

    def merge_forkers(self, pids: list[int]) -> ForkerUsage:
        """Adds the forkers' counters to ours.

        Returns:
          The forkers' own CPU time.
        """
        utime_s = 0.0
        stime_s = 0.0
        for pid in pids:
            p = Path(f"{self.stats_path}.{pid}")
            if p.is_file():  # Not if the forker was killed.
                other = json.loads(p.read_text())
                self.counters.add(other)
                utime_s += other["server_utime_s"]
                stime_s += other["server_stime_s"]
                p.unlink()
        return ForkerUsage(forkers_utime_s=utime_s, forkers_stime_s=stime_s)

    def write(self, extras: dict[str, Any], pid_suffix: bool) -> str:
        """Writes the stats line; returns it.

        With `pid_suffix` (a forker), writes to `<path>.<pid>` instead.
        """
        suffix = f".{os.getpid()}" if pid_suffix else ""
        stats = dataclasses.asdict(self.counters) | extras
        line = json.dumps(stats, sort_keys=True)
        Path(self.stats_path + suffix).write_text(line + "\n")
        return line
