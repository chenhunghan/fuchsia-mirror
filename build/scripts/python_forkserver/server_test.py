#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Integration tests for server.py, worker.py, and client.py."""

import concurrent.futures
import importlib.util
import json
import os
import resource
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from collections.abc import Callable, Mapping
from pathlib import Path
from typing import Any

from build.scripts.python_forkserver import client
from build.scripts.python_forkserver import server as forkserver
from build.scripts.python_forkserver import worker

_SRC_DIR = Path(__file__).resolve().parent
# How the build starts both Python actions and the server: served scripts run
# with the server's interpreter flags, so tests start all three the same way.
_PY_OPTS = ("-S", "-W", "error::SyntaxWarning")
# How long to wait for a process to exit. Generous: these tests run during
# the build, on CI builders that may be busy compiling.
_EXIT_TIMEOUT_S = 60

# Stand-ins for the RBE wrappers and the remote_action module they import,
# copied into each test's fake checkout (see their docstrings).
_TEST_DATA_DIR = _SRC_DIR / "test_data"
_FAKE_WRAPPER = _TEST_DATA_DIR / "fake_wrapper.py"
_FAKE_REMOTE_ACTION = _TEST_DATA_DIR / "remote_action.py"
# The variable through which build actions find the server.
_SOCKET_ENV = client.SOCKET_ENV.decode()


_FAKE_TOOL = """\
import json
import os
import sys
import warnings

import remote_action

if __name__ == "__main__":
    if sys.argv[1:] == ["raise"]:
        raise RuntimeError("boom-from-tool")
    print(json.dumps({
        "argv": sys.argv,
        "path0": sys.path[0],
        "name": __name__,
        "file": globals().get("__file__"),
        "who": getattr(remote_action, "WHO", "server copy"),
        "env_x": os.environ.get("X"),
        "optimize": sys.flags.optimize,
        "warnoptions": sys.warnoptions,
        "filters": [f[0] + ":" + f[2].__name__ for f in warnings.filters[:3]],
        "main_is_mod": sys.modules["__main__"].__dict__ is globals(),
    }, sort_keys=True))
"""


class ForkserverTest(unittest.TestCase):
    """Verifies behavioral equivalence, signal relay, concurrency, and failures."""

    def setUp(self) -> None:
        super().setUp()
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.tree = Path(self._tmp.name)
        self.rbe_dir = self.tree / "build" / "rbe"
        self.fs_dir = self.tree / "build" / "scripts" / "python_forkserver"
        self.work_dir = self.tree / "out" / "default"
        self.work_dir.mkdir(parents=True)

        files: dict[str, str | Path] = {
            f"build/scripts/python_forkserver/{name}": _SRC_DIR / name
            for name in (
                "server.py",
                "forker.py",
                "stats.py",
                "client.py",
                "worker.py",
            )
        }
        files["build/rbe/remote_action.py"] = _FAKE_REMOTE_ACTION
        for mod in (
            "cxx_remote_wrapper",
            "cxx_link_remote_wrapper",
            "rustc_remote_wrapper",
            "prebuilt_tool_remote_wrapper",
        ):
            files[f"build/rbe/{mod}.py"] = _FAKE_WRAPPER
        # Warm-up imports these too (see test_prewarm_modules_are_in_checkout).
        files["build/python/modules/serialization.py"] = ""
        files["build/scripts/service_profile.py"] = ""
        self._write_tree(self.tree, files)

        self.wrapper_rel = "../../build/rbe/cxx_remote_wrapper.py"
        self.sock_seq = 0

    @staticmethod
    def _write_tree(root: Path, files: Mapping[str, str | Path]) -> None:
        """Creates `files` under `root`, with any missing directories.

        Args:
          root: The directory to create the files in.
          files: Paths relative to `root`, each to its text, or to a Path to
            copy the file from.
        """
        for rel, content in files.items():
            path = root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(content, Path):
                shutil.copy2(content, path)
            else:
                path.write_text(content)

    def _base_env(self, **extra: str) -> dict[str, str]:
        env = dict(os.environ)
        env.pop(_SOCKET_ENV, None)
        # Under action tracing, fsatrace's preload opens an fd at startup.
        env.pop("LD_PRELOAD", None)
        env.pop(worker.ENV_PYTHONPATH, None)
        env[worker.ENV_DONT_WRITE_BYTECODE] = "1"
        env.update(extra)
        return env

    def _client_cmd(
        self, wrapper: str, args: list[str], py_opts: tuple[str, ...] = ()
    ) -> list[str]:
        # The build puts client.py in front of the script.
        launcher = str(self.fs_dir / "client.py")
        return [sys.executable, *_PY_OPTS, *py_opts, launcher, wrapper, *args]

    def _driver_cmd(self, setup: str, args: list[str]) -> list[str]:
        # Runs `setup`, then client.main() on the fake wrapper.
        return [
            sys.executable,
            "-S",
            "-c",
            (
                f"import posix, sys; sys.path.insert(0, {str(self.fs_dir)!r}); "
                f"import client; {setup}; "
                "sys.argv = [client.__file__] + sys.argv[1:]; "
                "client.main()"
            ),
            self.wrapper_rel,
            *args,
        ]

    def _run_direct(
        self,
        args: list[str],
        cwd: Path | None = None,
        wrapper: str | None = None,
        py_opts: tuple[str, ...] = (),
        env_vars: Mapping[str, str] | None = None,
        **extra_env: str,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                sys.executable,
                *_PY_OPTS,
                *py_opts,
                wrapper or self.wrapper_rel,
                *args,
            ],
            cwd=cwd or self.work_dir,
            capture_output=True,
            text=True,
            env=self._base_env(**(env_vars or {}), **extra_env),
        )

    def _run_client(
        self,
        args: list[str],
        sock_name: str | None,
        wrapper: str | None = None,
        cwd: Path | None = None,
        py_opts: tuple[str, ...] = (),
        env_vars: Mapping[str, str] | None = None,
        **extra_env: str,
    ) -> subprocess.CompletedProcess[str]:
        env = self._base_env(**(env_vars or {}), **extra_env)
        if sock_name is not None:
            env[_SOCKET_ENV] = sock_name
        return subprocess.run(
            self._client_cmd(wrapper or self.wrapper_rel, args, py_opts),
            cwd=cwd or self.work_dir,
            capture_output=True,
            text=True,
            env=env,
        )

    def _start_server(
        self, tag: str, forkers: int | None = 2
    ) -> tuple[subprocess.Popen[bytes], str, Path]:
        sock_name = self._sock_name(tag)
        stats_path = self.tree / f"stats_{tag}.json"
        args = ["--stats", str(stats_path)]
        if forkers is not None:
            args.extend(["--forkers", str(forkers)])
        proc = self._spawn_server(tag, sock_name, args)
        return proc, sock_name, stats_path

    def _sock_name(self, tag: str) -> str:
        self.sock_seq += 1
        return f"fx-python-forkserver-test-{tag}-{os.getpid()}-{self.sock_seq}-{time.time_ns()}"

    def _spawn_server(
        self,
        tag: str,
        sock_name: str,
        args: list[str],
        cwd: Path | None = None,
        ready: bytes = b"R",
        server: list[str] | None = None,
        **popen_kwargs: Any,
    ) -> subprocess.Popen[bytes]:
        """Starts server.py like forkserver_wrap.py and waits until it's ready.

        Asserts that the ready pipe gets `ready` and then EOF.

        Args:
          tag: Names the server's log file, `server_<tag>.log` in the tree.
          sock_name: The server's --socket-name.
          args: More server arguments.
          cwd: The server's working directory; the build directory by default.
          ready: What the ready pipe should get before EOF.
          server: What to run after the interpreter options, instead of
            server.py (it gets the same arguments).
          **popen_kwargs: More subprocess.Popen() arguments.
        """
        log_file = open(self.tree / f"server_{tag}.log", "wb")
        self.addCleanup(log_file.close)
        ready_r, ready_w = os.pipe()
        self.addCleanup(os.close, ready_r)
        popen_kwargs.setdefault("env", self._base_env())
        proc = subprocess.Popen(
            [
                sys.executable,
                *_PY_OPTS,
                *(server or [str(self.fs_dir / "server.py")]),
                "--socket-name",
                sock_name,
                "--ready-fd",
                str(ready_w),
                *args,
            ],
            cwd=cwd or self.work_dir,
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            pass_fds=(ready_w,),
            **popen_kwargs,
        )
        os.close(ready_w)
        self.addCleanup(self._cleanup_proc, proc)
        # One byte once the socket is listening, then EOF.
        self.assertEqual(os.read(ready_r, 2), ready)
        self.assertEqual(os.read(ready_r, 1), b"")
        return proc

    @staticmethod
    def _cleanup_proc(proc: subprocess.Popen[Any]) -> None:
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

    @staticmethod
    def _poll_until(
        predicate: Callable[[], bool], timeout_s: float = 10.0
    ) -> None:
        """Calls `predicate` every 10 ms until it returns True.

        Raises:
          TimeoutError: `predicate` didn't return True within `timeout_s`.
        """
        deadline = time.monotonic() + timeout_s
        while not predicate():
            if time.monotonic() >= deadline:
                raise TimeoutError(f"not true after {timeout_s} s: {predicate}")
            time.sleep(0.01)

    def _forker_pids(self, proc: subprocess.Popen[bytes], n: int) -> list[int]:
        """Returns the pids of the controller's `n` forkers, once forked."""
        # The kernel lists a thread's direct children here; the controller is
        # single-threaded, so these are all of its children (the forkers).
        children = Path(f"/proc/{proc.pid}/task/{proc.pid}/children")
        self._poll_until(lambda: len(children.read_text().split()) >= n)
        return [int(pid) for pid in children.read_text().split()]

    @staticmethod
    def _stop_server(proc: subprocess.Popen[bytes]) -> int:
        proc.send_signal(signal.SIGTERM)
        return proc.wait(timeout=_EXIT_TIMEOUT_S)

    def test_ready_byte_comes_after_listen_and_warm_up(self) -> None:
        # _start_server() returns once the server wrote its ready byte, so the
        # socket is already listening (__SO_ACCEPTCON in /proc/net/unix), and
        # warm-up is done.
        proc, sock, _ = self._start_server("ready", forkers=1)
        log = (self.tree / "server_ready.log").read_text()
        self.assertIn("python_forkserver: imported", log)
        listening = [
            line.split()
            for line in Path("/proc/net/unix").read_text().splitlines()
            if line.endswith(f" @{sock}")
        ]
        self.assertEqual([fields[3] for fields in listening], ["00010000"])
        self.assertEqual(self._run_client(["noop"], sock).returncode, 0)
        self.assertEqual(self._stop_server(proc), 0)

    def test_default_forkers_and_stats_merge(self) -> None:
        expected = min(8, os.cpu_count() or 1)
        self.assertEqual(forkserver._DEFAULT_FORKERS, expected)
        proc, sock, stats_path = self._start_server(
            "default_forkers", forkers=None
        )
        for i in range(12):
            res = self._run_client(["echo", str(i)], sock, X="df")
            self.assertEqual(res.returncode, 0)
        rc = self._stop_server(proc)
        self.assertEqual(rc, 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["forkers"], expected)
        self.assertEqual(stats["requests"], 12)
        self.assertEqual(stats["completed"], 12)
        self.assertNotIn("declined", stats)
        self.assertIn("forkers_utime_s", stats)
        self.assertIn("forkers_stime_s", stats)

    def test_behavioral_equivalence_with_direct_python(self) -> None:
        proc, sock, stats_path = self._start_server("equiv", forkers=2)
        cases: list[list[str]] = [
            ["echo", "a b", "c"],
            ["exit", "7"],
            ["sysexit", "5"],
            ["raise"],
            # Ctrl-C: atexit handlers and buffered output survive, and the
            # action dies from SIGINT.
            ["interrupt"],
            ["subproc", "hello"],
            ["asyncio"],
        ]
        for args in cases:
            with self.subTest(args=args):
                direct = self._run_direct(args, X="val1")
                via = self._run_client(args, sock, X="val1")
                self.assertEqual(via.returncode, direct.returncode)
                self.assertEqual(via.stdout, direct.stdout)
                if args[0] in ("raise", "interrupt"):
                    self.assertEqual(
                        via.stderr.strip().splitlines()[-1],
                        direct.stderr.strip().splitlines()[-1],
                    )
                    # Like the interpreter's, the traceback has no frames from
                    # the code that started the script.
                    self.assertNotIn("worker.py", via.stderr)
                else:
                    self.assertEqual(via.stderr, direct.stderr)

        out_direct = self.work_dir / "out_direct.txt"
        out_client = self.work_dir / "out_client.txt"
        self.assertEqual(
            self._run_direct(["write", str(out_direct)], X="w42").returncode, 0
        )
        self.assertEqual(
            self._run_client(
                ["write", str(out_client)], sock, X="w42"
            ).returncode,
            0,
        )
        self.assertEqual(out_client.read_text(), out_direct.read_text())
        self.assertEqual(out_client.read_text(), "written:w42\n")

        rc = self._stop_server(proc)
        self.assertEqual(rc, 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["completed"], 8)
        self.assertEqual(stats["exit_nonzero"], 3)
        self.assertEqual(stats["signaled"], 1)

    def _wait_ready(self, proc: subprocess.Popen[str]) -> None:
        assert proc.stdout is not None
        self.addCleanup(proc.stdout.close)
        self.assertEqual(proc.stdout.readline().strip(), "ready")

    def test_signal_forwarding_sigterm_and_sigint(self) -> None:
        proc, sock, _ = self._start_server("sig", forkers=2)

        env = self._base_env(**{_SOCKET_ENV: sock})
        # Signal arriving during handshake before worker PID is received.
        for sig in (signal.SIGTERM, signal.SIGINT):
            with self.subTest(phase="handshake", sig=sig):
                # Fakes a signal that arrived before the worker's pid.
                setup = (
                    "init = client._Messenger.__init__; "
                    "client._Messenger.__init__ = lambda self: init(self) or "
                    f"setattr(self, 'pending_signal', {int(sig)})"
                )
                res = subprocess.run(
                    self._driver_cmd(setup, ["sleep", "30"]),
                    cwd=self.work_dir,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    env=env,
                    timeout=_EXIT_TIMEOUT_S,
                )
                self.assertEqual(res.returncode, -int(sig))

        p_term = subprocess.Popen(
            self._client_cmd(self.wrapper_rel, ["ready_sleep", "30"]),
            cwd=self.work_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=env,
        )
        self.addCleanup(self._cleanup_proc, p_term)
        self._wait_ready(p_term)
        p_term.send_signal(signal.SIGTERM)
        rc_term = p_term.wait(timeout=_EXIT_TIMEOUT_S)
        self.assertEqual(rc_term, -signal.SIGTERM)

        p_int = subprocess.Popen(
            self._client_cmd(self.wrapper_rel, ["ready_sleep", "30"]),
            cwd=self.work_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=env,
        )
        self.addCleanup(self._cleanup_proc, p_int)
        d_int = subprocess.Popen(
            [sys.executable, "-S", self.wrapper_rel, "ready_sleep", "30"],
            cwd=self.work_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            env=self._base_env(),
        )
        self.addCleanup(self._cleanup_proc, d_int)
        self._wait_ready(p_int)
        self._wait_ready(d_int)
        p_int.send_signal(signal.SIGINT)
        d_int.send_signal(signal.SIGINT)
        self.assertEqual(
            p_int.wait(timeout=_EXIT_TIMEOUT_S),
            d_int.wait(timeout=_EXIT_TIMEOUT_S),
        )
        self.assertEqual(self._stop_server(proc), 0)

    def test_parallel_burst_across_forkers(self) -> None:
        proc, sock, stats_path = self._start_server("burst", forkers=4)
        with concurrent.futures.ThreadPoolExecutor(max_workers=32) as pool:
            rcs = list(
                pool.map(
                    lambda _: self._run_client(["noop"], sock).returncode,
                    range(160),
                )
            )
        self.assertTrue(all(r == 0 for r in rcs))
        self.assertEqual(self._stop_server(proc), 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["completed"], 160)
        self.assertEqual(stats["exit_nonzero"], 0)
        self.assertEqual(stats["signaled"], 0)

    def test_hard_failures_and_running_without_a_server(self) -> None:
        proc, sock, stats_path = self._start_server("failures", forkers=2)
        self.assertEqual(self._run_client(["echo", "ok"], sock).returncode, 0)

        # 1. Another working directory (e.g. a nested build) fails the action
        # without running it: warm-up computed paths from the server's.
        other_cwd = self.tree / "out" / "other"
        other_cwd.mkdir(parents=True)
        c_cwd = self._run_client(["echo", "cwd"], sock, cwd=other_cwd)
        self.assertEqual(c_cwd.returncode, 1)
        self.assertEqual(c_cwd.stdout, "")
        self.assertIn("runs actions from", c_cwd.stderr)
        self.assertIn(str(self.work_dir), c_cwd.stderr)

        # 2. Another script -> run from its file by the server, like Python.
        shutil.copy2(_FAKE_WRAPPER, self.rbe_dir / "unregistered_wrapper.py")
        extra_rel = "../../build/rbe/unregistered_wrapper.py"
        c_unk = self._run_client(
            ["echo", "unk"], sock, wrapper=extra_rel, X="u1"
        )
        d_unk = self._run_direct(["echo", "unk"], wrapper=extra_rel, X="u1")
        self.assertEqual(c_unk.returncode, 0)
        self.assertEqual(c_unk.stdout, d_unk.stdout)
        self.assertIn('"env_x": "u1"', c_unk.stdout)

        self.assertEqual(self._stop_server(proc), 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["completed"], 3)
        self.assertEqual(stats["exit_nonzero"], 1)

        # 3. Configured socket with no server listening -> fails the action and
        # tells the user how to turn the forkserver off.
        missing_target = self.work_dir / "missing_server.txt"
        c_nosock = self._run_client(
            ["write", str(missing_target)],
            "nonexistent-fx-python-forkserver-sock",
            X="ns",
        )
        self.assertEqual(c_nosock.returncode, 1)
        self.assertIn("isn't running", c_nosock.stderr)
        self.assertIn("python_forkserver", c_nosock.stderr)
        self.assertFalse(missing_target.exists())

        # 4. Unset FX_PYTHON_FORKSERVER_SOCKET -> execs plain Python.
        c_noenv = self._run_client(["echo", "ne"], None, X="ne")
        d_noenv = self._run_direct(["echo", "ne"], X="ne")
        self.assertEqual(c_noenv.returncode, d_noenv.returncode)
        self.assertEqual(c_noenv.stdout, d_noenv.stdout)

    def test_emfile_stops_the_server(self) -> None:
        sock = self._sock_name("emfile")

        def _limit_nofile() -> None:
            resource.setrlimit(resource.RLIMIT_NOFILE, (16, 16))

        proc = self._spawn_server(
            "emfile",
            sock,
            ["--stats", str(self.tree / "stats_emfile.json"), "--forkers", "1"],
            preexec_fn=_limit_nofile,
        )
        self.assertEqual(self._run_client(["echo", "warm"], sock).returncode, 0)

        # Each worker holds a connection fd in the forker until it exits, and
        # waits for its client's stdin to close, so these run out of fds well
        # before 24, however slowly the clients start.
        clients = [
            subprocess.Popen(
                self._client_cmd(self.wrapper_rel, ["wait_stdin"]),
                cwd=self.work_dir,
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=self._base_env(**{_SOCKET_ENV: sock}),
            )
            for _ in range(24)
        ]
        for c in clients:
            self.addCleanup(self._cleanup_proc, c)
        log = self.tree / "server_emfile.log"
        self._poll_until(
            lambda: "Too many open files" in log.read_text(),
            timeout_s=_EXIT_TIMEOUT_S,
        )
        for c in clients:
            assert c.stdin is not None
            c.stdin.close()
        for c in clients:
            self.assertIn(c.wait(timeout=_EXIT_TIMEOUT_S), (0, 1))
        # The forker exits on EMFILE, and the controller then stops.
        self.assertEqual(proc.wait(timeout=_EXIT_TIMEOUT_S), 1)
        after_target = self.work_dir / "after_emfile.txt"
        after = self._run_client(["write", str(after_target)], sock, X="em")
        self.assertEqual(after.returncode, 1)
        self.assertIn("isn't running", after.stderr)
        self.assertFalse(after_target.exists())

    def test_forker_exiting_early_stops_the_server(self) -> None:
        proc, sock, stats_path = self._start_server("forker_dies", forkers=2)
        self.assertEqual(self._run_client(["noop"], sock).returncode, 0)
        forkers = self._forker_pids(proc, 2)
        os.kill(forkers[0], signal.SIGKILL)
        self.assertEqual(proc.wait(timeout=_EXIT_TIMEOUT_S), 1)
        log = (self.tree / "server_forker_dies.log").read_text()
        self.assertIn(f"forker {forkers[0]} exited (-9)", log)
        self.assertIn("isn't running", self._run_client(["noop"], sock).stderr)
        # The other forker still reported its requests.
        self.assertTrue(stats_path.is_file())

    def test_prewarm_modules_are_in_checkout(self) -> None:
        # setUp stubs these in the test tree. Warm-up imports them with no
        # fallback, so if one moves in the checkout, this fails first.
        for name in worker._PREWARM_EXTRA_MODULES:
            self.assertIsNotNone(importlib.util.find_spec(name), name)

    def test_warm_up_failure_stops_the_server(self) -> None:
        modules_dir = self.tree / "build" / "python" / "modules"
        (modules_dir / "serialization.py").write_text(
            "raise RuntimeError('no')\n"
        )
        # It exits before the ready byte, so forkserver_wrap.py reports it.
        sock = self._sock_name("warm_fail")
        proc = self._spawn_server(
            "warm_fail",
            sock,
            ["--stats", str(self.tree / "stats_warm_fail.json")],
            ready=b"",
        )
        self.assertEqual(proc.wait(timeout=_EXIT_TIMEOUT_S), 1)
        log = (self.tree / "server_warm_fail.log").read_text()
        self.assertIn("RuntimeError: no", log)
        target = self.work_dir / "warm_fail.txt"
        res = self._run_client(["write", str(target)], sock)
        self.assertEqual(res.returncode, 1)
        self.assertIn("isn't running", res.stderr)
        self.assertFalse(target.exists())

    def test_unresponsive_server_timeout_and_client_gone_guard(self) -> None:
        proc, sock, stats_path = self._start_server("stopped", forkers=1)
        self.assertEqual(self._run_client(["echo", "warm"], sock).returncode, 0)
        [forker] = self._forker_pids(proc, 1)
        os.kill(forker, signal.SIGSTOP)
        try:
            target = self.work_dir / "once.txt"
            env = self._base_env(**{_SOCKET_ENV: sock}, X="once")
            res = subprocess.run(
                self._driver_cmd(
                    "client._HANDSHAKE_TIMEOUT_S = 0.2",
                    ["write", str(target)],
                ),
                cwd=self.work_dir,
                capture_output=True,
                text=True,
                env=env,
                timeout=10,
            )
            # A running server that doesn't answer fails the action instead
            # of silently running it in-process, and says how to opt out.
            self.assertEqual(res.returncode, 1)
            self.assertIn("didn't answer within 0.2 s", res.stderr)
            self.assertIn("python_forkserver", res.stderr)
            self.assertFalse(target.exists())
        finally:
            os.kill(forker, signal.SIGCONT)
        time.sleep(0.2)
        # The resumed server must not run the abandoned request either.
        self.assertFalse(target.exists())

        # Also test a client disconnecting right after sending its request
        # payload. The worker exits without running it if it sees EOF on `conn`
        # before it starts, but that's best effort: if it read the request
        # before the close arrived, it runs it, which is harmless because the
        # caller is gone. Either way, the server keeps serving.
        import socket as std_socket

        ghost_target = self.work_dir / "ghost.txt"
        ghost_script = self.work_dir / "ghost.py"
        ghost_script.write_text(
            'import sys\nopen(sys.argv[1], "w").write("ran")\n'
        )
        with std_socket.socket(std_socket.AF_UNIX, std_socket.SOCK_STREAM) as s:
            s.connect(b"\0" + sock.encode())
            self.assertEqual(s.recv(5)[:1], b"R")
            request = client.Request(
                os.fsencode(self.work_dir),
                0o022,
                [os.fsencode(ghost_script), os.fsencode(ghost_target)],
                [],
            )
            fds = b"".join(fd.to_bytes(4, sys.byteorder) for fd in (0, 1, 2))
            s.sendmsg(
                [request.encode()],
                [(std_socket.SOL_SOCKET, std_socket.SCM_RIGHTS, fds)],
            )
        self.assertEqual(self._run_client(["noop"], sock).returncode, 0)
        self.assertEqual(self._stop_server(proc), 0)
        stats = json.loads(stats_path.read_text())
        if not ghost_target.exists():
            self.assertGreaterEqual(
                stats["signaled"] + stats["exit_nonzero"], 1
            )

    def test_worker_sigkill(self) -> None:
        proc, sock, _ = self._start_server("sigkill", forkers=1)
        # Worker killed with SIGKILL -> client dies with SIGKILL (-9), no EINVAL traceback.
        killed = self._run_client(["self_sigkill"], sock)
        self.assertEqual(killed.returncode, -signal.SIGKILL)
        self.assertEqual(killed.stderr, "")
        self.assertEqual(self._stop_server(proc), 0)

    def test_sigint_stops_the_server_cleanly(self) -> None:
        # Like SIGTERM: the forkers stop, and the controller merges their stats.
        proc, sock, stats_path = self._start_server("sigint", forkers=2)
        self.assertEqual(self._run_client(["noop"], sock).returncode, 0)
        proc.send_signal(signal.SIGINT)
        self.assertEqual(proc.wait(timeout=_EXIT_TIMEOUT_S), 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["requests"], 1)
        self.assertEqual(list(self.tree.glob(f"{stats_path.name}.*")), [])
        log = (self.tree / "server_sigint.log").read_text()
        self.assertNotIn("Traceback", log)

    def test_client_hanging_up_before_its_request_is_not_an_error(self) -> None:
        # E.g. a client killed when the build is interrupted.
        import socket as std_socket

        proc, sock, _ = self._start_server("hangup", forkers=1)
        # Once right after the ready frame, and once with the PID frame
        # unread, which makes the close reset the connection.
        for leave_pid_unread in (False, True):
            with std_socket.socket(
                std_socket.AF_UNIX, std_socket.SOCK_STREAM
            ) as s:
                s.connect(b"\0" + sock.encode())
                self.assertEqual(s.recv(5)[:1], b"R")
                if leave_pid_unread:
                    self._poll_until(
                        lambda: len(s.recv(5, std_socket.MSG_PEEK)) == 5
                    )
        # The server keeps serving, and doesn't log a bad request.
        self.assertEqual(self._run_client(["noop"], sock).returncode, 0)
        self.assertEqual(self._stop_server(proc), 0)
        log = (self.tree / "server_hangup.log").read_text()
        self.assertNotIn("bad request", log)

    def test_stdout_line_buffering_follows_the_clients_stdout(self) -> None:
        # The server's stdout is a log file, so its sys.stdout isn't
        # line-buffered. Like with plain python3, a script whose stdout is a
        # terminal gets a line-buffered sys.stdout, and one on a pipe doesn't.
        # (test_apply_request_line_buffers_stdout_for_a_terminal checks the
        # same without a pty.)
        import pty

        try:
            primary, secondary = pty.openpty()
        except OSError as e:
            self.skipTest(f"no pty (e.g. in a build sandbox): {e}")
        self.addCleanup(os.close, primary)
        self.addCleanup(os.close, secondary)
        script = self.work_dir / "buffering.py"
        script.write_text(
            "import sys\nsys.stderr.write(str(sys.stdout.line_buffering))\n"
        )
        proc, sock, _ = self._start_server("buffering", forkers=1)
        runs = {
            "direct": ([sys.executable, *_PY_OPTS, str(script)], {}),
            "served": (self._client_cmd(str(script), []), {_SOCKET_ENV: sock}),
        }
        for name, (cmd, env_vars) in runs.items():
            res = subprocess.run(
                cmd,
                cwd=self.work_dir,
                stdin=subprocess.DEVNULL,
                stdout=secondary,
                stderr=subprocess.PIPE,
                text=True,
                env=self._base_env(**env_vars),
                timeout=_EXIT_TIMEOUT_S,
            )
            self.assertEqual(
                (name, res.returncode, res.stderr), (name, 0, "True")
            )
        piped = self._run_client([], sock, wrapper=str(script))
        self.assertEqual((piped.returncode, piped.stderr), (0, "False"))
        self.assertEqual(self._stop_server(proc), 0)

    def test_apply_request_line_buffers_stdout_for_a_terminal(self) -> None:
        # Build sandboxes have no ptys, so run apply_request() in a process
        # whose stdout is a pipe, with os.isatty() reporting a terminal on
        # fd 1 or not.
        code = (
            "import os, sys; "
            f"sys.path.insert(0, {str(self.fs_dir)!r}); "
            "import client, worker; "
            "tty = sys.argv[1] == 'tty'; "
            "os.isatty = lambda fd: tty and fd == 1; "
            "request = client.Request(os.getcwdb(), 0o022, [b'script'], []); "
            "worker.apply_request(request, [os.dup(fd) for fd in (0, 1, 2)]); "
            "sys.stderr.write(str(sys.stdout.line_buffering))"
        )
        for stdout, expected in (("tty", "True"), ("pipe", "False")):
            res = subprocess.run(
                [sys.executable, *_PY_OPTS, "-c", code, stdout],
                cwd=self.work_dir,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                env=self._base_env(),
                timeout=_EXIT_TIMEOUT_S,
            )
            self.assertEqual(
                (stdout, res.returncode, res.stderr), (stdout, 0, expected)
            )

    def test_server_ignores_clients_of_other_users(self) -> None:
        # The server is told it runs as uid + 1, so SO_PEERCRED reports every
        # client as another user's: it hangs up without running anything.
        sock = self._sock_name("uid")
        stats_path = self.tree / "stats_uid.json"
        proc = self._spawn_server(
            "uid",
            sock,
            ["--stats", str(stats_path), "--forkers", "1"],
            server=[
                "-c",
                (
                    f"import os, sys; sys.path[0] = {str(self.fs_dir)!r}; "
                    "uid = os.getuid(); os.getuid = lambda: uid + 1; "
                    "import server; sys.exit(server.main(sys.argv[1:]))"
                ),
            ],
        )
        target = self.work_dir / "other_user.txt"
        res = self._run_client(["write", str(target)], sock)
        self.assertEqual(res.returncode, 1)
        self.assertIn("closed the connection before answering", res.stderr)
        self.assertFalse(target.exists())
        self.assertEqual(self._stop_server(proc), 0)
        self.assertIn(
            "refused a client that runs as another user",
            (self.tree / "server_uid.log").read_text(),
        )
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["requests"], 0)

    def test_any_script_runs_like_python(self) -> None:
        # A script outside //build/rbe whose sibling module has the same name
        # as one the server pre-imported, a zip application with its own copy
        # of that module, an uncaught exception, and a compile error under
        # -W error::SyntaxWarning, served, against plain `python3`.
        self._write_tree(
            self.tree,
            {
                "build/tool/remote_action.py": "WHO = 'tool sibling'\n",
                "build/tool/tool.py": _FAKE_TOOL,
                "build/tool/bad_escape.py": 'print("\\d")\n',
                "app_src/__main__.py": _FAKE_TOOL,
                "app_src/remote_action.py": "WHO = 'zip copy'\n",
            },
        )
        import zipapp

        zipapp.create_archive(self.tree / "app_src", self.work_dir / "tool.pyz")

        proc, sock, stats_path = self._start_server("any", forkers=2)
        self.assertEqual(self._run_client(["noop"], sock).returncode, 0)
        tool = "../../build/tool/tool.py"
        cases: list[tuple[str, list[str]]] = [
            (tool, ["a b", "c"]),
            (tool, ["raise"]),
            ("tool.pyz", ["z"]),
            ("tool.pyz", ["raise"]),
            ("../../build/tool/bad_escape.py", []),
        ]
        for script, args in cases:
            with self.subTest(script=script, args=args):
                direct = self._run_direct(args, wrapper=script, X="t")
                via = self._run_client(args, sock, wrapper=script, X="t")
                self.assertEqual(via.returncode, direct.returncode)
                self.assertEqual(via.stdout, direct.stdout)
                self.assertEqual(via.stderr, direct.stderr)
        self.assertIn(
            "tool sibling", self._run_client([], sock, wrapper=tool).stdout
        )
        # Served scripts get the server's interpreter flags, not the client's.
        self.assertEqual(
            self._run_client(["o"], sock, wrapper=tool, py_opts=("-O",)).stdout,
            self._run_direct(["o"], wrapper=tool).stdout,
        )
        self.assertEqual(self._stop_server(proc), 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["completed"], len(cases) + 3)

    def test_warm_up_imports_leave_the_warning_filters_alone(self) -> None:
        # A module the server pre-imports that changes the warning filters.
        modules_dir = self.tree / "build" / "python" / "modules"
        (modules_dir / "serialization.py").write_text(
            "import warnings\nwarnings.simplefilter('ignore')\n"
        )
        warn_dir = self.tree / "build" / "warn"
        warn_dir.mkdir(parents=True)
        (warn_dir / "warn.py").write_text(
            "import warnings\nwarnings.warn('from-script')\nprint('done')\n"
        )
        proc, sock, _ = self._start_server("warnings", forkers=1)
        self.assertEqual(self._run_client(["noop"], sock).returncode, 0)
        script = "../../build/warn/warn.py"
        via = self._run_client([], sock, wrapper=script)
        direct = self._run_direct([], wrapper=script)
        self.assertIn("UserWarning: from-script", via.stderr)
        self.assertEqual(via.stderr, direct.stderr)
        self.assertEqual(via.stdout, "done\n")
        self.assertEqual(self._stop_server(proc), 0)

    def test_main_compile_writes_no_pycache_and_repeats_syntax_warning(
        self,
    ) -> None:
        # Even with PYTHONDONTWRITEBYTECODE unset (normal fx build), running a
        # __main__ script on the server must never create source-tree
        # __pycache__/*.pyc or unmarshal a cached .pyc that suppresses
        # -W error::SyntaxWarning on subsequent runs.
        src_dir = self.tree / "build" / "syntax_src"
        src_dir.mkdir(parents=True)
        clean_py = src_dir / "clean_script.py"
        clean_py.write_text("print('clean-ok')\n")
        bad_py = src_dir / "bad_escape.py"
        bad_py.write_text('x = "\\d"\nprint(x)\n')

        proc, sock, stats_path = self._start_server("pyc_warn", forkers=2)
        env_write_bytecode = self._base_env(**{_SOCKET_ENV: sock})
        env_write_bytecode.pop(worker.ENV_DONT_WRITE_BYTECODE, None)
        env_write_bytecode.pop(worker.ENV_PYCACHE_PREFIX, None)

        rel_clean = "../../build/syntax_src/clean_script.py"
        rel_bad = "../../build/syntax_src/bad_escape.py"

        # 1. Clean script + warning script -> zero __pycache__.
        r_clean = subprocess.run(
            self._client_cmd(rel_clean, []),
            cwd=self.work_dir,
            capture_output=True,
            text=True,
            env=env_write_bytecode,
        )
        self.assertEqual(r_clean.returncode, 0)
        self.assertEqual(r_clean.stdout, "clean-ok\n")
        r_bad = subprocess.run(
            self._client_cmd(rel_bad, []),
            cwd=self.work_dir,
            capture_output=True,
            text=True,
            env=env_write_bytecode,
        )
        self.assertEqual(r_bad.returncode, 1)
        self.assertIn("SyntaxError", r_bad.stderr)
        self.assertFalse((src_dir / "__pycache__").exists())

        # 2. Even if an external import previously wrote __pycache__/*.pyc, both
        # repeat runs under -W error::SyntaxWarning must fail like python3 -S.
        import py_compile
        import warnings

        # Pass cfile explicitly: py_compile honors this test process's
        # PYTHONPYCACHEPREFIX, which may be set when tests run in the build.
        pyc = (
            src_dir
            / "__pycache__"
            / (f"bad_escape.{sys.implementation.cache_tag}.pyc")
        )
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", SyntaxWarning)
            py_compile.compile(str(bad_py), cfile=str(pyc), doraise=False)
        self.assertTrue(any((src_dir / "__pycache__").glob("bad_escape*.pyc")))

        d_err = subprocess.run(
            [sys.executable, *_PY_OPTS, rel_bad],
            cwd=self.work_dir,
            capture_output=True,
            text=True,
            env={
                k: v for k, v in env_write_bytecode.items() if k != _SOCKET_ENV
            },
        )
        self.assertEqual(d_err.returncode, 1)
        for _ in range(2):
            v_err = subprocess.run(
                self._client_cmd(rel_bad, []),
                cwd=self.work_dir,
                capture_output=True,
                text=True,
                env=env_write_bytecode,
            )
            self.assertEqual(v_err.returncode, 1)
            self.assertEqual(v_err.stderr, d_err.stderr)

        self.assertEqual(self._stop_server(proc), 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["completed"], 4)
        self.assertEqual(stats["exit_nonzero"], 3)

    def test_pyz_stdlib_and_namespace_package_shadowing(self) -> None:
        # 1. .pyz bundling a module named `copy` (shadowing stdlib `copy`, not
        # in local_modules) must evict warm stdlib `copy` without calling
        # os.listdir on the .pyz file.
        # 2. A directory with `build.py` (like //build/go/build.py) must evict
        # the pre-warmed PEP 420 namespace package `build` when PYTHONPATH
        # includes the checkout root.
        self._write_tree(
            self.tree,
            {
                "build/scripts/service_profile.py": "LOADED = True\n",
                "build/go/build.py": "KIND = 'go_build_module'\n",
                "build/go/gen_meta.py": (
                    "import build\nprint(getattr(build, 'KIND', 'namespace'))\n"
                ),
                "pyz_stdlib_shadow/copy.py": "WHO = 'pyz_bundled_copy'\n",
                "pyz_stdlib_shadow/__main__.py": (
                    "import copy\nprint(getattr(copy, 'WHO', 'stdlib_copy'))\n"
                ),
            },
        )
        import zipapp

        pyz_path = self.work_dir / "shadow_copy.pyz"
        zipapp.create_archive(self.tree / "pyz_stdlib_shadow", pyz_path)

        # Start server with PYTHONPATH=self.tree (matching real fx build).
        pythonpath = {worker.ENV_PYTHONPATH: str(self.tree)}
        sock = self._sock_name("ns")
        stats_path = self.tree / "ns.stats.json"
        proc = self._spawn_server(
            "ns",
            sock,
            ["--stats", str(stats_path), "--forkers", "1"],
            env=self._base_env(**pythonpath),
        )
        self.assertEqual(
            self._run_client(["noop"], sock, env_vars=pythonpath).returncode,
            0,
        )

        for script, expected in (
            ("shadow_copy.pyz", "pyz_bundled_copy\n"),
            ("../../build/go/gen_meta.py", "go_build_module\n"),
        ):
            d_res = self._run_direct([], wrapper=script, env_vars=pythonpath)
            c_res = self._run_client(
                [], sock, wrapper=script, env_vars=pythonpath
            )
            self.assertEqual(c_res.returncode, 0, c_res.stderr)
            self.assertEqual(c_res.stdout, expected)
            self.assertEqual(c_res.stdout, d_res.stdout)

        # Direct guard check: forget_shadowed_modules never calls os.listdir
        # on a .pyz archive.
        import copy as std_copy
        from unittest import mock

        warm = worker.WarmState(
            tuple(sys.path[1:]), "/nonexistent", frozenset(), frozenset()
        )
        try:
            with mock.patch(
                "os.listdir",
                side_effect=AssertionError("os.listdir called on .pyz archive"),
            ):
                worker.forget_shadowed_modules(
                    str(pyz_path), warm.client_path, warm
                )
            self.assertNotIn("copy", sys.modules)
        finally:
            sys.modules["copy"] = std_copy

        self.assertEqual(self._stop_server(proc), 0)
        stats = json.loads(stats_path.read_text())
        self.assertEqual(stats["completed"], 3)


if __name__ == "__main__":
    unittest.main()
