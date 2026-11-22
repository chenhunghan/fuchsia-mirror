#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for build.scripts.lock_utils."""

import contextlib
import errno
import fcntl
import io
import multiprocessing
import os
import pathlib
import tempfile
import threading
import unittest
from unittest import mock

from build.profile import procfs_test_utils
from build.scripts import lock_utils


def _increment_file(path: pathlib.Path) -> int:
    if not path.exists():
        path.write_text("1", encoding="utf-8")
        return 1
    next_count = int(path.read_text(encoding="utf-8")) + 1
    path.write_text(f"{next_count}", encoding="utf-8")
    return next_count


def _locked_increment_file(path: pathlib.Path) -> int:
    lockfile = path.with_suffix(".lock")
    with lock_utils.BlockingFileLock(lockfile):
        return _increment_file(path)


class BuildLockPathTest(unittest.TestCase):
    def test_suffix_constant(self) -> None:
        self.assertEqual(lock_utils._BUILD_LOCK_SUFFIX, ".build_lock")

    def test_build_lock_path_standard_and_dotted_names(self) -> None:
        self.assertEqual(
            lock_utils.build_lock_path(pathlib.Path("/tmp/out/default")),
            pathlib.Path("/tmp/out/default.build_lock"),
        )
        self.assertEqual(
            lock_utils.build_lock_path(pathlib.Path("/tmp/out/default.arm64")),
            pathlib.Path("/tmp/out/default.arm64.build_lock"),
        )
        self.assertEqual(
            lock_utils.build_lock_path(pathlib.Path("out/default")),
            pathlib.Path("out/default.build_lock"),
        )
        self.assertEqual(
            lock_utils.build_lock_path(
                pathlib.Path("/tmp/out/default"),
                lock_suffix=".host_tool_lock",
            ),
            pathlib.Path("/tmp/out/default.host_tool_lock"),
        )


class BlockingFileLockTest(unittest.TestCase):
    def test_multiprocess_exclusion(self) -> None:
        worker_count = 50
        with tempfile.TemporaryDirectory() as td:
            count_file = pathlib.Path(td) / "count"
            try:
                with multiprocessing.Pool() as pool:
                    first_counts = pool.map(
                        _locked_increment_file, [count_file] * worker_count
                    )
            except OSError:  # in case /dev/shm is not writeable
                first_counts = list(
                    map(_locked_increment_file, [count_file] * worker_count)
                )

            self.assertSequenceEqual(
                sorted(first_counts), range(1, worker_count + 1)
            )

            # Run a second batch against the same lock file.
            try:
                with multiprocessing.Pool() as pool:
                    second_counts = pool.map(
                        _locked_increment_file, [count_file] * worker_count
                    )
            except OSError:
                second_counts = list(
                    map(_locked_increment_file, [count_file] * worker_count)
                )

            self.assertSequenceEqual(
                sorted(second_counts),
                range(worker_count + 1, 2 * worker_count + 1),
            )

    def test_creates_parent_directories_and_preserves_lockfile(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "nested" / "dir" / "tool.lock"
            with lock_utils.BlockingFileLock(lockfile) as lock:
                self.assertEqual(lock.lockfile, lockfile)
                self.assertFalse(lock.waited)
                self.assertTrue(lockfile.is_file())
            # Lockfile must not be unlinked (avoids flock inode race).
            self.assertTrue(lockfile.is_file())

    def test_uncontended_messages_to_stream_and_default_stdout(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "test.lock"
            stream = io.StringIO()
            with lock_utils.BlockingFileLock(
                lockfile,
                waiting_message="waiting...",
                acquired_message="acquired!",
                released_message="released!",
                message_stream=stream,
            ) as lock:
                self.assertFalse(lock.waited)
                self.assertEqual(stream.getvalue(), "acquired!\n")
            self.assertEqual(stream.getvalue(), "acquired!\nreleased!\n")

            stdout_buf = io.StringIO()
            with contextlib.redirect_stdout(stdout_buf):
                with lock_utils.BlockingFileLock(
                    lockfile,
                    waiting_message="waiting...",
                    acquired_message="Lock acquired, proceeding with build.",
                    released_message="Build completed.",
                ):
                    self.assertEqual(
                        stdout_buf.getvalue(),
                        "Lock acquired, proceeding with build.\n",
                    )
            self.assertEqual(
                stdout_buf.getvalue(),
                "Lock acquired, proceeding with build.\nBuild completed.\n",
            )

    @mock.patch.object(os, "getpid", return_value=1234, autospec=True)
    def test_write_pid_writes_and_clears_on_normal_and_exceptional_exit(
        self,
        _mock_getpid: mock.MagicMock,
    ) -> None:
        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "default.build_lock"
            with lock_utils.BlockingFileLock(lockfile, write_pid=True):
                self.assertEqual(lockfile.read_text(encoding="utf-8"), "1234\n")
            self.assertTrue(lockfile.is_file())
            self.assertEqual(lockfile.read_text(encoding="utf-8"), "")

            with self.assertRaises(RuntimeError):
                with lock_utils.BlockingFileLock(lockfile, write_pid=True):
                    self.assertEqual(
                        lockfile.read_text(encoding="utf-8"),
                        "1234\n",
                    )
                    raise RuntimeError("boom")
            self.assertTrue(lockfile.is_file())
            self.assertEqual(lockfile.read_text(encoding="utf-8"), "")

    @mock.patch.object(os, "getpid", return_value=1234, autospec=True)
    def test_real_contention_emits_waiting_message_and_sets_waited(
        self,
        _mock_getpid: mock.MagicMock,
    ) -> None:
        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "contended.lock"
            holder_fd = os.open(lockfile, os.O_RDWR | os.O_CREAT, 0o644)
            fcntl.flock(holder_fd, fcntl.LOCK_EX)

            entered_wait = threading.Event()
            stream = io.StringIO()
            orig_write = stream.write

            def notify_on_write(text: str) -> int:
                written = orig_write(text)
                if "waiting for holder" in text:
                    entered_wait.set()
                return written

            stream.write = notify_on_write  # type: ignore[method-assign]

            def release_holder() -> None:
                entered_wait.wait(timeout=5.0)
                fcntl.flock(holder_fd, fcntl.LOCK_UN)
                os.close(holder_fd)

            releaser = threading.Thread(target=release_holder, daemon=True)
            releaser.start()
            try:
                with lock_utils.BlockingFileLock(
                    lockfile,
                    write_pid=True,
                    waiting_message="waiting for holder...",
                    acquired_message="acquired after wait",
                    released_message="released after wait",
                    message_stream=stream,
                ) as lock:
                    self.assertTrue(lock.waited)
                    self.assertEqual(
                        lockfile.read_text(encoding="utf-8"),
                        "1234\n",
                    )
            finally:
                releaser.join(timeout=5.0)

            self.assertSequenceEqual(
                stream.getvalue().splitlines(),
                (
                    "waiting for holder...",
                    "acquired after wait",
                    "released after wait",
                ),
            )
            self.assertEqual(lockfile.read_text(encoding="utf-8"), "")

    @mock.patch.object(lock_utils, "fcntl", autospec=True, spec_set=True)
    def test_contended_mock_sequence_and_exception_cleanup(
        self, mock_fcntl: mock.MagicMock
    ) -> None:
        mock_fcntl.LOCK_EX = fcntl.LOCK_EX
        mock_fcntl.LOCK_NB = fcntl.LOCK_NB
        mock_fcntl.LOCK_UN = fcntl.LOCK_UN
        flock_ops: list[int] = []

        def fake_flock(_fd: int, op: int) -> None:
            flock_ops.append(op)
            if op == (fcntl.LOCK_EX | fcntl.LOCK_NB):
                raise BlockingIOError("busy")

        mock_fcntl.flock.side_effect = fake_flock
        stream = io.StringIO()

        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "mock.lock"
            with self.assertRaises(RuntimeError):
                with lock_utils.BlockingFileLock(
                    lockfile,
                    waiting_message="Waiting...",
                    acquired_message="Acquired.",
                    released_message="Released.",
                    message_stream=stream,
                ) as lock:
                    self.assertTrue(lock.waited)
                    raise RuntimeError("inner failure")

        self.assertSequenceEqual(
            flock_ops,
            (
                fcntl.LOCK_EX | fcntl.LOCK_NB,
                fcntl.LOCK_EX,
                fcntl.LOCK_UN,
            ),
        )
        self.assertSequenceEqual(
            stream.getvalue().splitlines(),
            ("Waiting...", "Acquired.", "Released."),
        )

    @mock.patch.object(lock_utils, "fcntl", autospec=True, spec_set=True)
    def test_non_contention_oserror_propagates_without_waiting_message(
        self, mock_fcntl: mock.MagicMock
    ) -> None:
        mock_fcntl.LOCK_EX = fcntl.LOCK_EX
        mock_fcntl.LOCK_NB = fcntl.LOCK_NB
        mock_fcntl.LOCK_UN = fcntl.LOCK_UN
        mock_fcntl.flock.side_effect = OSError(errno.ENOLCK, "no locks")
        stream = io.StringIO()

        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "err.lock"
            with self.assertRaises(OSError) as ctx:
                with lock_utils.BlockingFileLock(
                    lockfile,
                    waiting_message="Should not print",
                    acquired_message="Should not print",
                    message_stream=stream,
                ):
                    pass

        self.assertEqual(ctx.exception.errno, errno.ENOLCK)
        self.assertEqual(stream.getvalue(), "")
        mock_fcntl.flock.assert_called_once_with(
            mock.ANY, fcntl.LOCK_EX | fcntl.LOCK_NB
        )

    @mock.patch.object(lock_utils, "os", autospec=True, spec_set=True)
    @mock.patch.object(lock_utils, "fcntl", autospec=True, spec_set=True)
    def test_fd_zero_is_unlocked_and_closed(
        self, mock_fcntl: mock.MagicMock, mock_os: mock.MagicMock
    ) -> None:
        mock_fcntl.LOCK_EX = fcntl.LOCK_EX
        mock_fcntl.LOCK_NB = fcntl.LOCK_NB
        mock_fcntl.LOCK_UN = fcntl.LOCK_UN
        mock_os.O_RDWR = os.O_RDWR
        mock_os.O_CREAT = os.O_CREAT
        mock_os.open.return_value = 0

        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "fd0.lock"
            with lock_utils.BlockingFileLock(lockfile):
                pass

        mock_fcntl.flock.assert_any_call(0, fcntl.LOCK_UN)
        mock_os.close.assert_called_once_with(0)

    @mock.patch.object(lock_utils, "os", autospec=True, spec_set=True)
    @mock.patch.object(lock_utils, "fcntl", autospec=True, spec_set=True)
    def test_enter_failure_clears_partial_pid_unlocks_and_closes(
        self, mock_fcntl: mock.MagicMock, mock_os: mock.MagicMock
    ) -> None:
        mock_fcntl.LOCK_EX = fcntl.LOCK_EX
        mock_fcntl.LOCK_NB = fcntl.LOCK_NB
        mock_fcntl.LOCK_UN = fcntl.LOCK_UN
        mock_os.O_RDWR = os.O_RDWR
        mock_os.O_CREAT = os.O_CREAT
        mock_os.SEEK_SET = os.SEEK_SET
        mock_os.open.return_value = 7
        mock_os.getpid.return_value = 1234
        mock_os.write.side_effect = [2, OSError(errno.ENOSPC, "disk full")]

        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "enospc.lock"
            with self.assertRaises(OSError) as ctx:
                with lock_utils.BlockingFileLock(lockfile, write_pid=True):
                    pass

        self.assertEqual(ctx.exception.errno, errno.ENOSPC)
        # First ftruncate is before write; second clears partial PID on failure.
        self.assertEqual(mock_os.ftruncate.call_count, 2)
        mock_fcntl.flock.assert_any_call(7, fcntl.LOCK_UN)
        mock_os.close.assert_called_once_with(7)

    @mock.patch.object(lock_utils, "os", autospec=True, spec_set=True)
    @mock.patch.object(lock_utils, "fcntl", autospec=True, spec_set=True)
    def test_enter_failure_preserves_original_error_when_cleanup_clear_fails(
        self, mock_fcntl: mock.MagicMock, mock_os: mock.MagicMock
    ) -> None:
        mock_fcntl.LOCK_EX = fcntl.LOCK_EX
        mock_fcntl.LOCK_NB = fcntl.LOCK_NB
        mock_fcntl.LOCK_UN = fcntl.LOCK_UN
        mock_os.O_RDWR = os.O_RDWR
        mock_os.O_CREAT = os.O_CREAT
        mock_os.SEEK_SET = os.SEEK_SET
        mock_os.open.return_value = 7
        mock_os.getpid.return_value = 1234
        mock_os.write.side_effect = OSError(errno.ENOSPC, "disk full")
        mock_os.ftruncate.side_effect = [None, OSError(errno.EIO, "io error")]

        with tempfile.TemporaryDirectory() as td:
            lockfile = pathlib.Path(td) / "eio.lock"
            with self.assertRaises(OSError) as ctx:
                with lock_utils.BlockingFileLock(lockfile, write_pid=True):
                    pass

        self.assertEqual(ctx.exception.errno, errno.ENOSPC)
        mock_fcntl.flock.assert_any_call(7, fcntl.LOCK_UN)
        mock_os.close.assert_called_once_with(7)


class BuildLockInspectionTest(unittest.TestCase):
    def test_is_ancestor_pid_matches_pid_1_self(self) -> None:
        def getpid_1() -> int:
            return 1

        self.assertTrue(lock_utils._is_ancestor_pid("1", getpid_fn=getpid_1))
        self.assertTrue(lock_utils._is_ancestor_pid(1, getpid_fn=getpid_1))
        self.assertFalse(lock_utils._is_ancestor_pid("42", getpid_fn=getpid_1))
        self.assertFalse(lock_utils._is_ancestor_pid("0", getpid_fn=getpid_1))
        self.assertFalse(lock_utils._is_ancestor_pid("-5", getpid_fn=getpid_1))
        self.assertFalse(lock_utils._is_ancestor_pid("", getpid_fn=getpid_1))
        self.assertFalse(
            lock_utils._is_ancestor_pid("not-a-pid", getpid_fn=getpid_1)
        )

    def test_is_ancestor_pid_walks_proc_chain_through_pid_1(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            fake_proc = pathlib.Path(td) / "proc"
            procfs_test_utils.make_fake_process(
                fake_proc, 42, comm="python3 (test)", ppid=10
            )
            procfs_test_utils.make_fake_process(
                fake_proc, 10, comm="fx-build", ppid=1
            )
            # Process injected into a PID namespace (ppid == 0) is not a
            # descendant of PID 1.
            procfs_test_utils.make_fake_process(
                fake_proc, 43, comm="exec-tool", ppid=0
            )

            self.assertTrue(
                lock_utils._is_ancestor_pid(
                    "42", proc_root=fake_proc, getpid_fn=lambda: 42
                )
            )
            self.assertTrue(
                lock_utils._is_ancestor_pid(
                    "10", proc_root=fake_proc, getpid_fn=lambda: 42
                )
            )
            self.assertTrue(
                lock_utils._is_ancestor_pid(
                    "1", proc_root=fake_proc, getpid_fn=lambda: 42
                )
            )
            self.assertFalse(
                lock_utils._is_ancestor_pid(
                    "99", proc_root=fake_proc, getpid_fn=lambda: 42
                )
            )

            self.assertTrue(
                lock_utils._is_ancestor_pid(
                    "43", proc_root=fake_proc, getpid_fn=lambda: 43
                )
            )
            self.assertFalse(
                lock_utils._is_ancestor_pid(
                    "1", proc_root=fake_proc, getpid_fn=lambda: 43
                )
            )

    @mock.patch.object(os, "getpid", return_value=1234, autospec=True)
    def test_read_live_pid_lifecycle(
        self,
        _mock_getpid: mock.MagicMock,
    ) -> None:
        def alive_kill(_pid: int, _sig: int) -> None:
            return None

        def dead_kill(_pid: int, _sig: int) -> None:
            raise ProcessLookupError

        def eperm_kill(_pid: int, _sig: int) -> None:
            raise PermissionError

        def einval_kill(_pid: int, _sig: int) -> None:
            raise OSError(errno.EINVAL, "invalid")

        with tempfile.TemporaryDirectory() as td:
            lock_path = pathlib.Path(td) / "custom.lock"
            self.assertIsNone(
                lock_utils._read_live_pid(lock_path, kill_fn=alive_kill)
            )

            with lock_utils.BlockingFileLock(lock_path, write_pid=True):
                self.assertEqual(
                    lock_utils._read_live_pid(lock_path, kill_fn=alive_kill),
                    "1234",
                )
            # Clean release truncates the file, so no active PID is reported.
            self.assertIsNone(
                lock_utils._read_live_pid(lock_path, kill_fn=alive_kill)
            )

            # Non-numeric or zero PID content returns None.
            lock_path.write_text("not-a-pid\n", encoding="utf-8")
            self.assertIsNone(
                lock_utils._read_live_pid(lock_path, kill_fn=alive_kill)
            )
            lock_path.write_text("0\n", encoding="utf-8")
            self.assertIsNone(
                lock_utils._read_live_pid(lock_path, kill_fn=alive_kill)
            )

            # Dead PID (ESRCH) returns None; EPERM means the PID is alive under
            # another UID; unexpected OSError propagates.
            lock_path.write_text("999999\n", encoding="utf-8")
            self.assertIsNone(
                lock_utils._read_live_pid(lock_path, kill_fn=dead_kill)
            )
            self.assertEqual(
                lock_utils._read_live_pid(lock_path, kill_fn=eperm_kill),
                "999999",
            )
            with self.assertRaises(OSError) as ctx:
                lock_utils._read_live_pid(lock_path, kill_fn=einval_kill)
            self.assertEqual(ctx.exception.errno, errno.EINVAL)

    def test_find_ancestor_and_active_lock_pid(self) -> None:
        def alive_kill(_pid: int, _sig: int) -> None:
            return None

        with tempfile.TemporaryDirectory() as td:
            fake_proc = pathlib.Path(td) / "proc"
            procfs_test_utils.make_fake_process(
                fake_proc, 42, comm="child", ppid=10
            )
            procfs_test_utils.make_fake_process(
                fake_proc, 10, comm="parent", ppid=1
            )
            lock_path = pathlib.Path(td) / "custom_tool.lock"

            lock_path.write_text("10\n", encoding="utf-8")
            self.assertEqual(
                lock_utils.find_ancestor_lock_pid(
                    lock_path,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                ),
                "10",
            )
            self.assertIsNone(
                lock_utils.find_active_lock_pid(
                    lock_path,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                )
            )

            lock_path.write_text("99\n", encoding="utf-8")
            self.assertIsNone(
                lock_utils.find_ancestor_lock_pid(
                    lock_path,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                )
            )
            self.assertEqual(
                lock_utils.find_active_lock_pid(
                    lock_path,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                ),
                "99",
            )

    def test_find_ancestor_and_active_build_pid(self) -> None:
        def alive_kill(_pid: int, _sig: int) -> None:
            return None

        self.assertIsNone(lock_utils.find_ancestor_build_pid(None))
        self.assertIsNone(lock_utils.find_active_build_pid(None))

        with tempfile.TemporaryDirectory() as td:
            fake_proc = pathlib.Path(td) / "proc"
            procfs_test_utils.make_fake_process(
                fake_proc, 42, comm="child", ppid=10
            )
            procfs_test_utils.make_fake_process(
                fake_proc, 10, comm="parent", ppid=1
            )

            build_dir = pathlib.Path(td) / "out" / "default"
            build_dir.mkdir(parents=True)

            # Legacy inside-build-dir lock file is intentionally ignored.
            (build_dir / ".build_lock").write_text("99\n", encoding="utf-8")
            self.assertIsNone(
                lock_utils.find_active_build_pid(
                    build_dir,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                )
            )

            lock_path = lock_utils.build_lock_path(build_dir)
            lock_path.write_text("10\n", encoding="utf-8")
            self.assertEqual(
                lock_utils.find_ancestor_build_pid(
                    build_dir,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                ),
                "10",
            )
            self.assertIsNone(
                lock_utils.find_active_build_pid(
                    build_dir,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                )
            )

            lock_path.write_text("99\n", encoding="utf-8")
            self.assertIsNone(
                lock_utils.find_ancestor_build_pid(
                    build_dir,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                )
            )
            self.assertEqual(
                lock_utils.find_active_build_pid(
                    build_dir,
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                ),
                "99",
            )

            # Custom lock_suffix selects an independent lock file.
            custom_lock = lock_utils.build_lock_path(
                build_dir, lock_suffix=".host_tool_lock"
            )
            custom_lock.write_text("10\n", encoding="utf-8")
            self.assertEqual(
                lock_utils.find_ancestor_build_pid(
                    build_dir,
                    lock_suffix=".host_tool_lock",
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                ),
                "10",
            )
            self.assertIsNone(
                lock_utils.find_active_build_pid(
                    build_dir,
                    lock_suffix=".host_tool_lock",
                    proc_root=fake_proc,
                    getpid_fn=lambda: 42,
                    kill_fn=alive_kill,
                )
            )


if __name__ == "__main__":
    unittest.main()
