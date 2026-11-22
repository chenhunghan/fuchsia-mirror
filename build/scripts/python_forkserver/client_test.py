#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit tests for client.py: the wire protocol and the client's side of it.

The launcher tests talk to a fake server in this process, so they cover the
client without server.py; server_test.py covers the two together.
"""

import os
import pathlib
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from collections.abc import Callable

from build.scripts.python_forkserver import client

_SRC_DIR = pathlib.Path(__file__).resolve().parent
_CLIENT = str(_SRC_DIR / "client.py")


def _msg(tag: bytes, value: int) -> bytes:
    return tag + value.to_bytes(4, "little", signed=True)


class RequestTest(unittest.TestCase):
    def test_round_trip(self) -> None:
        # Not 0o022, which Request.current() sets while it reads the umask.
        self.addCleanup(os.umask, os.umask(0o027))
        argv = ["script.py", "a", "b c", ""]
        data = client.Request.current(argv).encode()
        self.assertEqual(int.from_bytes(data[:4], "little"), len(data) - 4)

        request = client.Request.decode(data[4:])
        self.assertEqual(os.umask(0o027), 0o027)  # Restored.
        self.assertEqual(request.cwd, os.getcwdb())
        self.assertEqual(request.umask, 0o027)
        self.assertEqual(request.argv, [os.fsencode(a) for a in argv])
        self.assertEqual(
            request.env, [k + b"=" + v for k, v in os.environb.items()]
        )

    def test_round_trip_without_environment(self) -> None:
        request = client.Request(b"/out", 0o027, [b"s.py", b""], [])
        decoded = client.Request.decode(request.encode()[4:])
        self.assertEqual(
            (decoded.cwd, decoded.umask, decoded.argv, decoded.env),
            (b"/out", 0o027, [b"s.py", b""], []),
        )

    def test_malformed_payloads_raise_value_error(self) -> None:
        good = client.Request(b"/out", 0o22, [b"s.py"], [b"X=1"])
        fields = good.encode()[4:].split(b"\0")
        cases = {
            "empty": [b""],
            "wrong magic": [b"fx-python-forkserver-v1", *fields[1:]],
            "too short": fields[:3],
            "bad umask": [*fields[:2], b"x", *fields[3:]],
            "umask out of range": [*fields[:2], b"512", *fields[3:]],
            "bad count": [*fields[:3], b"x", *fields[4:]],
            "count past end": [*fields[:3], b"999", *fields[4:]],
            "no argv": [*fields[:3], b"0", *fields[4:]],
            "negative count": [*fields[:3], b"-1", *fields[4:]],
        }
        for name, case in cases.items():
            with self.subTest(name), self.assertRaises(ValueError):
                client.Request.decode(b"\0".join(case))


class _FakeServer:
    """Accepts one connection and answers it with `respond`."""

    def __init__(
        self, test: unittest.TestCase, respond: Callable[[socket.socket], None]
    ) -> None:
        self.name = f"fx-python-forkserver-client-test-{os.getpid()}-{id(self)}"
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(b"\0" + self.name.encode())
        self.listener.listen(1)
        test.addCleanup(self.listener.close)
        self.request: bytes = b""
        self.thread = threading.Thread(target=self._serve, args=(respond,))
        self.thread.start()
        test.addCleanup(self.thread.join, 10)

    def _serve(self, respond: Callable[[socket.socket], None]) -> None:
        conn, _ = self.listener.accept()
        with conn:
            conn.sendall(_msg(client.TAG_READY, 0))
            data, fds, _, _ = socket.recv_fds(conn, 1 << 20, 3)
            for fd in fds:
                os.close(fd)
            self.request = data
            respond(conn)


class LauncherTest(unittest.TestCase):
    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = pathlib.Path(tmp.name)
        self.marker = self.dir / "ran.txt"
        # The action: writes its other arguments to the file named by its first,
        # so tests can see that it ran, and with which arguments.
        self.script = self.dir / "script.py"
        self.script.write_text(
            "import sys\n"
            "open(sys.argv[1], 'w').write(' '.join(sys.argv[2:]))\n"
        )

    def _env(self, **extra: str) -> dict[str, str]:
        env = dict(os.environ)
        # LD_PRELOAD: under action tracing, fsatrace's preload opens an fd at
        # startup, which would take the closed-stdio slot in these children.
        for key in ("FX_PYTHON_FORKSERVER_SOCKET", "LD_PRELOAD", "PYTHONPATH"):
            env.pop(key, None)
        env.update(extra)
        return env

    def _cmd(self) -> list[str]:
        return [
            sys.executable,
            "-S",
            _CLIENT,
            str(self.script),
            str(self.marker),
            "a",
            "b",
        ]

    def _run(self, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            self._cmd(),
            cwd=self.dir,
            capture_output=True,
            text=True,
            env=self._env(**env),
            timeout=30,
        )

    def test_execs_python_without_a_socket_and_warns_once(self) -> None:
        marker = self.dir / ".python_forkserver_slow_path"
        first = self._run()
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(self.marker.read_text(), "a b")
        self.assertIn("no Python forkserver is running", first.stderr)
        self.assertTrue(marker.exists())
        # Later actions in the same directory run quietly, until something
        # (`fx build`) deletes the marker.
        self.marker.unlink()
        second = self._run()
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(second.stderr, "")
        self.assertEqual(self.marker.read_text(), "a b")

    def test_exec_keeps_the_interpreter_options(self) -> None:
        self.script.write_text(
            "import sys\n"
            "print(sys.flags.no_site, sys.flags.optimize, sys.argv)\n"
        )
        res = subprocess.run(
            [sys.executable, "-S", "-O", _CLIENT, str(self.script), "x"],
            cwd=self.dir,
            capture_output=True,
            text=True,
            env=self._env(),
            timeout=30,
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout, f"1 1 [{str(self.script)!r}, 'x']\n")

    def test_closed_stdio_fails_the_action(self) -> None:
        res = subprocess.run(
            self._cmd(),
            cwd=self.dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=self._env(
                FX_PYTHON_FORKSERVER_SOCKET=f"fx-python-forkserver-unused-{os.getpid()}"
            ),
            preexec_fn=lambda: os.close(0),
            timeout=30,
        )
        self.assertEqual(res.returncode, 1)
        self.assertIn("stdin, stdout, or stderr is closed", res.stderr)
        self.assertFalse(self.marker.exists())

    def test_missing_server_fails_the_action(self) -> None:
        res = self._run(
            FX_PYTHON_FORKSERVER_SOCKET=f"fx-python-forkserver-unused-{os.getpid()}"
        )
        self.assertEqual(res.returncode, 1)
        self.assertIn("isn't running", res.stderr)
        self.assertIn(".fx/config/python_forkserver", res.stderr)
        self.assertFalse(self.marker.exists())

    def test_exits_with_the_worker_status(self) -> None:
        fake = _FakeServer(
            self, lambda conn: conn.sendall(_msg(client.TAG_STATUS, 3 << 8))
        )
        res = self._run(FX_PYTHON_FORKSERVER_SOCKET=fake.name)
        self.assertEqual(res.returncode, 3, res.stderr)
        self.assertFalse(self.marker.exists())
        request = client.Request.decode(fake.request[4:])
        self.assertEqual(request.cwd, os.fsencode(self.dir))
        self.assertEqual(
            request.argv,
            [os.fsencode(a) for a in self._cmd()[3:]],
        )

    def test_dies_from_the_worker_signal(self) -> None:
        fake = _FakeServer(
            self,
            lambda conn: conn.sendall(_msg(client.TAG_STATUS, signal.SIGTERM)),
        )
        res = self._run(FX_PYTHON_FORKSERVER_SOCKET=fake.name)
        self.assertEqual(res.returncode, -signal.SIGTERM, res.stderr)

    def test_server_closing_before_the_worker_starts_fails_it(self) -> None:
        fake = _FakeServer(self, lambda conn: None)
        res = self._run(FX_PYTHON_FORKSERVER_SOCKET=fake.name)
        self.assertEqual(res.returncode, 1)
        self.assertIn("before starting the action", res.stderr)
        self.assertFalse(self.marker.exists())

    def test_server_closing_mid_action_fails_it(self) -> None:
        fake = _FakeServer(
            self, lambda conn: conn.sendall(_msg(client.TAG_PID, 2**31 - 1))
        )
        res = self._run(FX_PYTHON_FORKSERVER_SOCKET=fake.name)
        self.assertEqual(res.returncode, 1)
        self.assertIn("while the action was running", res.stderr)

    def test_forwards_signals_to_the_worker(self) -> None:
        sent_pid = threading.Event()

        def respond(conn: socket.socket) -> None:
            # sys.executable, not `sleep`: build actions may have no PATH.
            worker = subprocess.Popen(
                [sys.executable, "-S", "-c", "import time; time.sleep(30)"],
                process_group=0,
            )
            conn.sendall(_msg(client.TAG_PID, worker.pid))
            sent_pid.set()
            status = worker.wait(timeout=20)
            conn.sendall(_msg(client.TAG_STATUS, -status))

        fake = _FakeServer(self, respond)
        proc = subprocess.Popen(
            self._cmd(),
            cwd=self.dir,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=self._env(FX_PYTHON_FORKSERVER_SOCKET=fake.name),
        )
        self.assertTrue(sent_pid.wait(20))
        proc.send_signal(signal.SIGTERM)
        self.assertEqual(proc.wait(timeout=20), -signal.SIGTERM)


if __name__ == "__main__":
    unittest.main()
