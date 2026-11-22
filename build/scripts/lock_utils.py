#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared file-locking and `.build_lock` inspection utilities."""

import contextlib
import fcntl
import os
import pathlib
from collections.abc import Callable
from typing import Any, Self, TextIO

from build.profile import procfs

_MAX_PID_ANCESTRY_HOPS = 64

# LINT.IfChange(build_lock)
_BUILD_LOCK_SUFFIX = ".build_lock"
# LINT.ThenChange(//tools/devshell/lib/vars.sh:build_lock)


def _write_pid_to_fd(fd: int, pid: int) -> None:
    os.lseek(fd, 0, os.SEEK_SET)
    os.ftruncate(fd, 0)
    payload = f"{pid}\n".encode("ascii")
    view = memoryview(payload)
    written = 0
    while written < len(payload):
        written += os.write(fd, view[written:])


def _clear_fd(fd: int) -> None:
    os.lseek(fd, 0, os.SEEK_SET)
    os.ftruncate(fd, 0)


def _emit_message(message: str | None, stream: TextIO | None) -> None:
    if message is None:
        return
    if stream is None:
        print(message, flush=True)
    else:
        print(message, file=stream, flush=True)


def _is_ancestor_pid(
    want_pid: str | int,
    *,
    proc_root: pathlib.Path = procfs.DEFAULT_PROC_ROOT,
    getpid_fn: Callable[[], int] = os.getpid,
) -> bool:
    """Return True if `want_pid` is the current process or an ancestor process."""
    if isinstance(want_pid, bool):
        return False
    if isinstance(want_pid, int):
        target = want_pid
    elif want_pid and want_pid.isdigit():
        target = int(want_pid)
    else:
        return False
    if target <= 0:
        return False

    cur = getpid_fn()
    hops = 0
    while cur > 0 and hops < _MAX_PID_ANCESTRY_HOPS:
        # Check cur == target before stopping at PID 1 so processes running as
        # (or descended from) PID 1 inside a PID namespace / container match.
        if cur == target:
            return True
        if cur == 1:
            break
        try:
            stat = procfs.read_process_stat(cur, proc_root=proc_root)
        except OSError:
            break
        if stat is None or stat.ppid <= 0 or stat.ppid == cur:
            break
        cur = stat.ppid
        hops += 1
    return False


def _read_live_pid(
    lock_file: pathlib.Path,
    *,
    kill_fn: Callable[[int, int], None] = os.kill,
) -> str | None:
    """Return the live PID recorded in `lock_file`, or None."""
    if not lock_file.is_file():
        return None
    try:
        tokens = lock_file.read_text(encoding="utf-8").strip().split()
    except OSError:
        return None
    if not tokens or not tokens[0].isdigit():
        return None
    pid_str = tokens[0]
    pid = int(pid_str)
    if pid <= 0:
        return None
    try:
        kill_fn(pid, 0)
    except ProcessLookupError:
        return None
    except PermissionError:
        # EPERM means the process is alive under another UID.
        return pid_str
    return pid_str


def build_lock_path(
    build_dir: pathlib.Path,
    lock_suffix: str = _BUILD_LOCK_SUFFIX,
) -> pathlib.Path:
    """Return the lock file path corresponding to `build_dir`.

    Args:
        build_dir: Path to the Fuchsia build output directory (e.g.,
            `out/default`).
        lock_suffix: Suffix appended to `build_dir.name` to form the sibling
            lock filename. Defaults to `.build_lock`.

    Returns:
        Path to the sibling lock file next to `build_dir`.
    """
    return build_dir.parent / f"{build_dir.name}{lock_suffix}"


class BlockingFileLock:
    """Blocking mutual-exclusion file lock backed by `fcntl.flock`.

    Never unlinks the lock file on release to avoid the `flock` unlink race.
    When `write_pid` is True, records the owning PID while the lock is held and
    truncates the file before releasing the lock.
    """

    def __init__(
        self,
        lockfile: pathlib.Path,
        *,
        write_pid: bool = False,
        waiting_message: str | None = None,
        acquired_message: str | None = None,
        released_message: str | None = None,
        message_stream: TextIO | None = None,
    ) -> None:
        """Initialize a blocking file lock.

        Args:
            lockfile: Path to the file used for `fcntl.flock` coordination.
            write_pid: If True, write the current process PID while holding the
                lock and truncate the file before releasing.
            waiting_message: Optional diagnostic message printed only when lock
                acquisition blocks on another holder.
            acquired_message: Optional diagnostic message printed once the lock
                is acquired.
            released_message: Optional diagnostic message printed after
                releasing the lock.
            message_stream: Optional stream for diagnostic messages; defaults to
                `sys.stdout`.
        """
        self._lockfile = lockfile
        self._write_pid = write_pid
        self._waiting_message = waiting_message
        self._acquired_message = acquired_message
        self._released_message = released_message
        self._message_stream = message_stream
        self._lockfile_fd: int | None = None
        self._waited = False

    @property
    def lockfile(self) -> pathlib.Path:
        return self._lockfile

    @property
    def waited(self) -> bool:
        return self._waited

    def _acquire(self) -> None:
        assert self._lockfile_fd is None, "BlockingFileLock is not re-entrant"
        # Ensure the parent directory exists and open or create the lock file.
        self._lockfile.parent.mkdir(parents=True, exist_ok=True)
        lockfile_fd = os.open(self._lockfile, os.O_RDWR | os.O_CREAT, 0o644)
        acquired = False
        pid_written = False
        try:
            # Attempt a non-blocking exclusive lock first so waiting diagnostics
            # are only emitted when another holder currently owns the lock.
            try:
                fcntl.flock(lockfile_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                # Lock is held; notify caller and block until acquired.
                _emit_message(self._waiting_message, self._message_stream)
                fcntl.flock(lockfile_fd, fcntl.LOCK_EX)
                waited = True
            else:
                waited = False
            acquired = True
            # Persist the owning PID while holding the lock, if requested.
            if self._write_pid:
                pid_written = True
                _write_pid_to_fd(lockfile_fd, os.getpid())
            _emit_message(self._acquired_message, self._message_stream)
            self._lockfile_fd = lockfile_fd
            self._waited = waited
        finally:
            # If setup failed before ownership was recorded, clear any partially
            # written PID, release the flock if acquired, and close the fd.
            if self._lockfile_fd is None:
                try:
                    if acquired:
                        try:
                            if pid_written:
                                with contextlib.suppress(OSError):
                                    _clear_fd(lockfile_fd)
                        finally:
                            fcntl.flock(lockfile_fd, fcntl.LOCK_UN)
                finally:
                    os.close(lockfile_fd)

    def _release(self) -> None:
        lockfile_fd = self._lockfile_fd
        if lockfile_fd is None:
            return
        self._lockfile_fd = None
        try:
            if self._write_pid:
                _clear_fd(lockfile_fd)
        finally:
            try:
                fcntl.flock(lockfile_fd, fcntl.LOCK_UN)
            finally:
                os.close(lockfile_fd)
        # Do not remove the lock file:
        # https://stackoverflow.com/questions/17708885/flock-removing-locked-file-without-race-condition
        _emit_message(self._released_message, self._message_stream)

    def __enter__(self) -> Self:
        self._acquire()
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self._release()


def find_ancestor_lock_pid(
    lock_path: pathlib.Path,
    *,
    proc_root: pathlib.Path = procfs.DEFAULT_PROC_ROOT,
    getpid_fn: Callable[[], int] = os.getpid,
    kill_fn: Callable[[int, int], None] = os.kill,
) -> str | None:
    """Return the live PID in `lock_path` if it belongs to an ancestor process.

    Args:
        lock_path: Path to the PID-bearing lock file to inspect.
        proc_root: Base directory for procfs lookups (defaults to `/proc`).
        getpid_fn: Callable returning the current PID (defaults to `os.getpid`).
        kill_fn: Callable used to probe PID liveness (defaults to `os.kill`).

    Returns:
        The live PID string if it is the current process or an ancestor, else
        None.
    """
    if (
        pid_str := _read_live_pid(lock_path, kill_fn=kill_fn)
    ) is not None and _is_ancestor_pid(
        pid_str, proc_root=proc_root, getpid_fn=getpid_fn
    ):
        return pid_str
    return None


def find_active_lock_pid(
    lock_path: pathlib.Path,
    *,
    proc_root: pathlib.Path = procfs.DEFAULT_PROC_ROOT,
    getpid_fn: Callable[[], int] = os.getpid,
    kill_fn: Callable[[int, int], None] = os.kill,
) -> str | None:
    """Return the live PID in `lock_path` if it belongs to a non-ancestor process.

    Args:
        lock_path: Path to the PID-bearing lock file to inspect.
        proc_root: Base directory for procfs lookups (defaults to `/proc`).
        getpid_fn: Callable returning the current PID (defaults to `os.getpid`).
        kill_fn: Callable used to probe PID liveness (defaults to `os.kill`).

    Returns:
        The live PID string if it belongs to another active process, else None.
    """
    if (
        pid_str := _read_live_pid(lock_path, kill_fn=kill_fn)
    ) is not None and not _is_ancestor_pid(
        pid_str, proc_root=proc_root, getpid_fn=getpid_fn
    ):
        return pid_str
    return None


def find_ancestor_build_pid(
    build_dir: pathlib.Path | None,
    *,
    lock_suffix: str = _BUILD_LOCK_SUFFIX,
    proc_root: pathlib.Path = procfs.DEFAULT_PROC_ROOT,
    getpid_fn: Callable[[], int] = os.getpid,
    kill_fn: Callable[[int, int], None] = os.kill,
) -> str | None:
    """Return the live build lock PID if it belongs to an ancestor process.

    Args:
        build_dir: Path to the build output directory, or None.
        lock_suffix: Suffix appended to `build_dir.name` to locate the lock
            file.
        proc_root: Base directory for procfs lookups (defaults to `/proc`).
        getpid_fn: Callable returning the current PID (defaults to `os.getpid`).
        kill_fn: Callable used to probe PID liveness (defaults to `os.kill`).

    Returns:
        The live ancestor PID string if found, else None.
    """
    if build_dir is None:
        return None
    return find_ancestor_lock_pid(
        build_lock_path(build_dir, lock_suffix=lock_suffix),
        proc_root=proc_root,
        getpid_fn=getpid_fn,
        kill_fn=kill_fn,
    )


def find_active_build_pid(
    build_dir: pathlib.Path | None,
    *,
    lock_suffix: str = _BUILD_LOCK_SUFFIX,
    proc_root: pathlib.Path = procfs.DEFAULT_PROC_ROOT,
    getpid_fn: Callable[[], int] = os.getpid,
    kill_fn: Callable[[int, int], None] = os.kill,
) -> str | None:
    """Return the live build lock PID if it belongs to a non-ancestor process.

    Args:
        build_dir: Path to the build output directory, or None.
        lock_suffix: Suffix appended to `build_dir.name` to locate the lock
            file.
        proc_root: Base directory for procfs lookups (defaults to `/proc`).
        getpid_fn: Callable returning the current PID (defaults to `os.getpid`).
        kill_fn: Callable used to probe PID liveness (defaults to `os.kill`).

    Returns:
        The live non-ancestor PID string if found, else None.
    """
    if build_dir is None:
        return None
    return find_active_lock_pid(
        build_lock_path(build_dir, lock_suffix=lock_suffix),
        proc_root=proc_root,
        getpid_fn=getpid_fn,
        kill_fn=kill_fn,
    )
