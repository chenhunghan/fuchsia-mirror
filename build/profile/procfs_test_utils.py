#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared test helpers for constructing fake Linux procfs trees."""

from collections.abc import Sequence
from pathlib import Path


def make_fake_process(
    proc_root: Path,
    pid: int,
    *,
    comm: str = "test",
    ppid: int = 1,
    pgrp: int | None = None,
    state: str = "S",
    utime: int = 0,
    stime: int = 0,
    cmdline: Sequence[str] | None = None,
    wchan: str | None = None,
    fds: int = 0,
) -> Path:
    """Creates a mock /proc/<pid> directory populated with virtual files.

    Args:
        proc_root: Base directory acting as the fake `/proc` filesystem root.
        pid: Process ID to create under `proc_root`.
        comm: Process executable name written into the `stat` file.
        ppid: Parent process ID written into the `stat` file.
        pgrp: Process group ID written into `stat`; defaults to `pid`.
        state: Single-character process state written into `stat`.
        utime: User-mode CPU time ticks written into `stat`.
        stime: Kernel-mode CPU time ticks written into `stat`.
        cmdline: Optional argv sequence written as NUL-separated bytes to
            `cmdline`.
        wchan: Optional wait-channel string written to `wchan`.
        fds: Number of placeholder file descriptor entries to create under `fd/`.

    Returns:
        Path to the created fake `/proc/<pid>` directory.
    """
    pid_dir = proc_root / str(pid)
    pid_dir.mkdir(parents=True, exist_ok=True)

    actual_pgrp = pgrp if pgrp is not None else pid
    stat_content = f"{pid} ({comm}) {state} {ppid} {actual_pgrp} 0 0 0 0 0 0 0 0 {utime} {stime} 0 0\n"
    (pid_dir / "stat").write_text(stat_content, encoding="utf-8")

    if cmdline is not None:
        (pid_dir / "cmdline").write_bytes(
            b"\0".join(arg.encode("utf-8") for arg in cmdline)
            + (b"\0" if cmdline else b"")
        )

    if wchan is not None:
        (pid_dir / "wchan").write_text(f"{wchan}\n", encoding="utf-8")

    if fds > 0:
        fd_dir = pid_dir / "fd"
        fd_dir.mkdir(parents=True, exist_ok=True)
        for fd in range(fds):
            (fd_dir / str(fd)).touch()

    return pid_dir
